#!/usr/bin/env python3
# Differential test for agents/extensions, the unix socket API, the
# klippysocket bridge, database backups and status subscriptions.
#
# usage: ext_test.py
import os
import json
import time
import socket
import asyncio
import requests
import tornado.websocket
from difftest import REF, RUST, REF_DIR, RUST_DIR, norm, diff

async def ws_open(host, path="/websocket"):
    return await tornado.websocket.websocket_connect(f"ws://{host}{path}")

class Client:
    def __init__(self, conn):
        self.conn = conn
        self.rid = 100
        self.events = []
        self.pending = {}
        self.calls = []
        self.task = asyncio.get_event_loop().create_task(self._reader())

    async def _reader(self):
        while True:
            msg = await self.conn.read_message()
            if msg is None:
                return
            data = json.loads(msg)
            if "id" in data and ("result" in data or "error" in data):
                fut = self.pending.pop(data["id"], None)
                if fut is not None:
                    fut.set_result(data)
            elif "method" in data and "id" in data:
                self.calls.append(data)
            elif "method" in data:
                if data["method"] not in ("notify_proc_stat_update", "notify_gcode_response"):
                    self.events.append(data)

    async def call(self, method, params=None):
        self.rid += 1
        msg = {"jsonrpc": "2.0", "method": method, "id": self.rid}
        if params is not None:
            msg["params"] = params
        fut = asyncio.get_event_loop().create_future()
        self.pending[self.rid] = fut
        await self.conn.write_message(json.dumps(msg))
        return await asyncio.wait_for(fut, 10)

    def take(self, method=None):
        ev = [e for e in self.events if method is None or e["method"] == method]
        self.events = [e for e in self.events if not (method is None or e["method"] == method)]
        return ev

def http(host, method, path, **kw):
    r = requests.request(method, f"http://{host}{path}", timeout=20, **kw)
    try:
        body = r.json()
    except Exception:
        body = r.text
    return {"status": r.status_code, "body": body}

def uds_rpc(sock_path, msgs):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(sock_path)
    out = []
    buf = b""
    for m in msgs:
        s.sendall(json.dumps(m).encode() + b"\x03")
        while True:
            while b"\x03" not in buf:
                buf += s.recv(65536)
            raw, buf = buf.split(b"\x03", 1)
            data = json.loads(raw)
            if data.get("id") == m.get("id"):
                out.append(data)
                break
    s.close()
    return out

async def run(host, inst_dir):
    res = {}
    obs = Client(await ws_open(host))
    await obs.call("server.connection.identify", {"client_name": "observer", "version": "1", "type": "web", "url": "x"})
    agent = Client(await ws_open(host))
    res["agent_identify"] = await agent.call("server.connection.identify", {"client_name": "testagent", "version": "2.0", "type": "agent", "url": "http://agent"})
    agent2 = Client(await ws_open(host))
    res["agent_dup"] = await agent2.call("server.connection.identify", {"client_name": "testagent", "version": "2.0", "type": "agent", "url": "http://agent"})
    res["web_send_event"] = await obs.call("connection.send_event", {"event": "x"})
    await asyncio.sleep(0.3)
    res["obs_events_connected"] = obs.take("notify_agent_event")
    res["agent_self_events"] = agent.take("notify_agent_event")
    res["send_event"] = await agent.call("connection.send_event", {"event": "my_event", "data": {"a": [1, 2], "b": "c"}})
    res["send_reserved"] = await agent.call("connection.send_event", {"event": "connected"})
    await asyncio.sleep(0.3)
    res["obs_events_custom"] = obs.take("notify_agent_event")
    res["register_method"] = await agent.call("connection.register_remote_method", {"method_name": "agent_test_method"})
    res["register_dup"] = await agent.call("connection.register_remote_method", {"method_name": "agent_test_method"})
    res["register_web"] = await obs.call("connection.register_remote_method", {"method_name": "web_method"})
    res["list"] = http(host, "GET", "/server/extensions/list")
    # extension request answered by the agent
    loop = asyncio.get_event_loop()
    fut = loop.run_in_executor(None, lambda: http(host, "POST", "/server/extensions/request", json={"agent": "testagent", "method": "do_thing", "arguments": {"x": 1}}))
    for _ in range(50):
        if agent.calls:
            break
        await asyncio.sleep(0.1)
    call = agent.calls.pop(0) if agent.calls else None
    res["agent_call_recd"] = {k: v for k, v in (call or {}).items() if k != "id"}
    if call:
        await agent.conn.write_message(json.dumps({"jsonrpc": "2.0", "result": {"done": True}, "id": call["id"]}))
    res["ext_request"] = await fut
    fut = loop.run_in_executor(None, lambda: http(host, "POST", "/server/extensions/request", json={"agent": "testagent", "method": "fail_thing"}))
    for _ in range(50):
        if agent.calls:
            break
        await asyncio.sleep(0.1)
    call = agent.calls.pop(0) if agent.calls else None
    if call:
        await agent.conn.write_message(json.dumps({"jsonrpc": "2.0", "error": {"code": 77, "message": "nope", "data": {"z": 1}}, "id": call["id"]}))
    res["ext_request_err"] = await fut
    res["ext_request_err_ws"] = None
    fut2 = loop.create_task(obs.call("server.extensions.request", {"agent": "testagent", "method": "fail_ws"}))
    for _ in range(50):
        if agent.calls:
            break
        await asyncio.sleep(0.1)
    call = agent.calls.pop(0) if agent.calls else None
    if call:
        await agent.conn.write_message(json.dumps({"jsonrpc": "2.0", "error": {"code": 77, "message": "nope", "data": [1]}, "id": call["id"]}))
    res["ext_request_err_ws"] = await fut2
    res["ext_bad_args"] = http(host, "POST", "/server/extensions/request", json={"agent": "testagent", "method": "x", "arguments": 5})
    res["ext_no_agent"] = http(host, "POST", "/server/extensions/request", json={"agent": "none", "method": "x"})
    # klippy calls the agent method
    res["gcode_remote"] = http(host, "POST", "/printer/gcode/script", json={"script": "TEST_AGENT_METHOD"})
    await asyncio.sleep(0.5)
    res["agent_klippy_calls"] = [{k: v for k, v in c.items() if k != "id"} for c in agent.calls]
    res["agent_klippy_events"] = agent.take()
    agent.conn.close()
    await asyncio.sleep(0.5)
    res["obs_events_disconnect"] = obs.take("notify_agent_event")
    res["list_after"] = http(host, "GET", "/server/extensions/list")
    agent2.conn.close()
    # unix socket
    sock = os.path.join(inst_dir, "comms", "moonraker.sock")
    res["uds"] = uds_rpc(sock, [
        {"jsonrpc": "2.0", "method": "server.connection.identify", "params": {"client_name": "uds", "version": "1", "type": "other", "url": "x"}, "id": 1},
        {"jsonrpc": "2.0", "method": "server.websocket.id", "id": 2},
        {"jsonrpc": "2.0", "method": "printer.info", "id": 3},
        {"jsonrpc": "2.0", "method": "printer.objects.query", "params": {"objects": {"webhooks": None}}, "id": 4},
        {"jsonrpc": "2.0", "method": "server.files.roots", "id": 5},
        {"jsonrpc": "2.0", "method": "access.user", "id": 6},
    ])
    # klippysocket bridge
    try:
        br = await ws_open(host, "/klippysocket")
        await br.write_message(json.dumps({"id": 7, "method": "objects/query", "params": {"objects": {"webhooks": None}}}))
        msg = await asyncio.wait_for(br.read_message(), 5)
        res["bridge"] = json.loads(msg)
        br.close()
    except Exception as e:
        res["bridge"] = f"error: {e}"
    # subscriptions: which objects/fields get pushed
    sub = Client(await ws_open(host))
    res["subscribe"] = await sub.call("printer.objects.subscribe", {"objects": {"toolhead": ["position", "homed_axes"], "gcode_move": None, "webhooks": None}})
    http(host, "POST", "/printer/gcode/script", json={"script": "G28\nG1 X50 Y50 F6000\nM400"})
    await asyncio.sleep(2.0)
    shapes = set()
    for e in sub.take("notify_status_update"):
        for obj, fields in e["params"][0].items():
            for f in fields:
                shapes.add(f"{obj}.{f}")
    res["status_fields"] = sorted(shapes)
    res["subscribe_none"] = await sub.call("printer.objects.subscribe", {"objects": {}})
    http(host, "POST", "/printer/gcode/script", json={"script": "G1 X60 Y60 F6000\nM400"})
    await asyncio.sleep(1.0)
    res["status_after_unsub"] = len(sub.take("notify_status_update"))
    sub.conn.close()
    obs.conn.close()
    # database backups
    res["bkp_create"] = http(host, "POST", "/server/database/backup", json={"filename": "test-bkp.db"})
    res["bkp_bad"] = http(host, "POST", "/server/database/backup", json={"filename": "../x.db"})
    res["db_list"] = http(host, "GET", "/server/database/list")
    res["compact"] = http(host, "POST", "/server/database/compact")
    res["bkp_delete"] = http(host, "DELETE", "/server/database/backup?filename=test-bkp.db")
    res["bkp_delete_missing"] = http(host, "DELETE", "/server/database/backup?filename=test-bkp.db")
    res["restore_missing"] = http(host, "POST", "/server/database/restore", json={"filename": "nope.db"})
    res["debug_table"] = http(host, "GET", "/debug/database/table?table=job_totals")
    return res

VOLATILE = {"connection_id", "websocket_id", "free", "used", "total", "eventtime", "process_id", "size",
            "estimated_print_time", "print_time", "position", "gcode_position", "homing_origin", "speed",
            "speed_factor", "extrude_factor", "absolute_coordinates", "absolute_extrude", "homed_axes",
            "backup_path", "previous_size", "new_size", "uptime"}

def vol(v, key=None):
    if key in VOLATILE:
        return "<volatile>"
    if isinstance(v, dict):
        return {k: vol(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [vol(x, key) for x in v]
    if isinstance(v, str):
        return v.replace(REF_DIR, "<D>").replace(RUST_DIR, "<D>")
    return v

def main():
    a = asyncio.run(run(REF, REF_DIR))
    b = asyncio.run(run(RUST, RUST_DIR))
    bad = 0
    for k in a:
        d = diff(vol(norm(a[k])), vol(norm(b.get(k))), f"${k}")
        if d:
            bad += 1
            print(f"=== {k}")
            for line in d[:20]:
                print("   ", line)
    print(f"\n{len(a) - bad}/{len(a)} checks identical")

if __name__ == "__main__":
    main()
