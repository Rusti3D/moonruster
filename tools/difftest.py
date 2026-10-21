#!/usr/bin/env python3
# Differential tester: sends identical requests to the reference moonraker
# and to moonruster, normalizes volatile values and reports differences.
#
# usage: difftest.py [case-filter]
import sys
import json
import re
import asyncio
import requests
import tornado.websocket

REF = "localhost:7010"
RUST = "localhost:7020"
REF_DIR = "/root/mr/inst/ref"
RUST_DIR = "/root/mr/inst/rust"

VOLATILE_KEYS = {
    "eventtime", "time", "websocket_id", "connection_id", "system_uptime",
    "process_id", "uuid", "created_on", "time_added", "time_in_queue",
    "modified", "start_time", "end_time", "estimated_print_time",
    "print_time", "print_duration", "total_duration", "uptime", "free",
    "used", "total", "date", "cpu_usage", "memory", "network", "cpu_temp",
    "system_cpu_usage", "system_memory", "moonraker_stats", "instance_id",
    "token", "refresh_token", "api_key", "available", "bandwidth",
    "rx_bytes", "tx_bytes", "rx_packets", "tx_packets", "temperature",
    "temperatures", "targets", "powers", "speeds", "pressure_advance",
    "smooth_time", "progress", "file_position", "ssl_port",
}

def norm(v, key=None):
    if key in VOLATILE_KEYS:
        return "<volatile>"
    if isinstance(v, dict):
        return {k: norm(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [norm(x, key) for x in v]
    if isinstance(v, str):
        v = v.replace(REF_DIR, "<D>").replace(RUST_DIR, "<D>")
        v = v.replace("7010", "<PORT>").replace("7020", "<PORT>")
        v = re.sub(r"Traceback.*", "<traceback>", v, flags=re.S)
        return v
    return v

def diff(a, b, path="$"):
    out = []
    if type(a) != type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool) and not isinstance(b, bool)):
        out.append(f"{path}: type {type(a).__name__} != {type(b).__name__}: {json.dumps(a)[:200]} vs {json.dumps(b)[:200]}")
        return out
    if isinstance(a, dict):
        ka, kb = list(a.keys()), list(b.keys())
        for k in ka:
            if k not in b:
                out.append(f"{path}.{k}: missing in rust (ref={json.dumps(a[k])[:120]})")
        for k in kb:
            if k not in a:
                out.append(f"{path}.{k}: extra in rust (rust={json.dumps(b[k])[:120]})")
        common = [k for k in ka if k in b]
        if [k for k in kb if k in a] != common:
            out.append(f"{path}: key order differs: {common} vs {[k for k in kb if k in a]}")
        for k in common:
            out.extend(diff(a[k], b[k], f"{path}.{k}"))
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: list len {len(a)} != {len(b)}: {json.dumps(a)[:300]} vs {json.dumps(b)[:300]}")
        for i, (x, y) in enumerate(zip(a, b)):
            out.extend(diff(x, y, f"{path}[{i}]"))
    elif a != b:
        if isinstance(a, float) or isinstance(b, float):
            if type(a) != type(b):
                out.append(f"{path}: numeric type {a!r} vs {b!r}")
                return out
        out.append(f"{path}: {json.dumps(a)[:200]} != {json.dumps(b)[:200]}")
    return out

def http(host, method, path, **kw):
    r = requests.request(method, f"http://{host}{path}", timeout=30, **kw)
    try:
        body = r.json()
    except Exception:
        body = r.text
    hdrs = {k.lower(): v for k, v in r.headers.items() if k.lower() in ("content-type", "location", "content-disposition", "access-control-allow-origin", "accept-ranges", "content-range")}
    return {"status": r.status_code, "reason": r.reason, "headers": hdrs, "body": body}

HTTP_CASES = [
    ("GET", "/server/info", {}),
    ("GET", "/server/info?raw=true", {}),
    ("GET", "/server/config", {}),
    ("GET", "/server/nothing", {}),
    ("DELETE", "/server/info", {}),
    ("GET", "/printer/info", {}),
    ("GET", "/printer/objects/list", {}),
    ("GET", "/printer/objects/query?webhooks&print_stats", {}),
    ("GET", "/printer/objects/query?toolhead=position,max_velocity", {}),
    ("POST", "/printer/objects/query", {"json": {"objects": {"webhooks": None}}}),
    ("GET", "/printer/query_endpoints/list", {}),
    ("POST", "/printer/gcode/script?script=G90", {}),
    ("POST", "/printer/gcode/script?script=BADCMD", {}),
    ("GET", "/printer/gcode/help", {}),
    ("GET", "/server/database/list", {}),
    ("GET", "/server/database/item?namespace=foo&key=bar", {}),
    ("POST", "/server/database/item", {"json": {"namespace": "test", "key": "a.b.c", "value": [1, 2.5, "x", True, None, {"q": 1}]}}),
    ("GET", "/server/database/item?namespace=test&key=a", {}),
    ("GET", "/server/database/item?namespace=test", {}),
    ("POST", "/server/database/item?namespace=test&key=num&value:int=5", {}),
    ("POST", "/server/database/item?namespace=test&key=flt&value:float=5.5", {}),
    ("POST", "/server/database/item?namespace=test&key=bl&value:bool=true", {}),
    ("POST", "/server/database/item?namespace=test&key=js&value:json=%7B%22a%22%3A1%7D", {}),
    ("GET", "/server/database/item?namespace=test", {}),
    ("DELETE", "/server/database/item?namespace=test&key=a.b", {}),
    ("DELETE", "/server/database/item?namespace=test&key=nope", {}),
    ("GET", "/server/database/item?namespace=moonraker", {}),
    ("POST", "/server/database/item?namespace=moonraker&key=x&value=1", {}),
    ("GET", "/server/database/item?namespace=database", {}),
    ("GET", "/server/files/roots", {}),
    ("GET", "/server/files/list", {}),
    ("GET", "/server/files/list?root=config", {}),
    ("GET", "/server/files/directory?path=config", {}),
    ("GET", "/server/files/directory?path=gcodes&extended=true", {}),
    ("GET", "/server/files/directory?path=nope", {}),
    ("POST", "/server/files/directory?path=gcodes/testdir", {}),
    ("POST", "/server/files/directory?path=gcodes/testdir", {}),
    ("GET", "/server/files/metadata?filename=nope.gcode", {}),
    ("GET", "/server/files/thumbnails?filename=nope.gcode", {}),
    ("DELETE", "/server/files/directory?path=gcodes/testdir", {}),
    ("GET", "/server/files/config/printer.cfg", {}),
    ("GET", "/server/files/config/nope.cfg", {}),
    ("GET", "/server/temperature_store", {}),
    ("GET", "/server/gcode_store?count=2", {}),
    ("GET", "/machine/system_info", {}),
    ("GET", "/machine/proc_stats", {}),
    ("GET", "/machine/sudo/info", {}),
    ("GET", "/machine/peripherals/serial", {}),
    ("GET", "/machine/peripherals/usb", {}),
    ("GET", "/machine/peripherals/video", {}),
    ("GET", "/access/info", {}),
    ("GET", "/access/user", {}),
    ("GET", "/access/users/list", {}),
    ("GET", "/access/oneshot_token", {}),
    ("POST", "/access/login", {"json": {"username": "nobody", "password": "x"}}),
    ("GET", "/server/history/list", {}),
    ("GET", "/server/history/totals", {}),
    ("GET", "/server/history/job?uid=000099", {}),
    ("GET", "/server/job_queue/status", {}),
    ("POST", "/server/job_queue/job?filenames=nope.gcode", {}),
    ("GET", "/server/announcements/list", {}),
    ("GET", "/server/announcements/feeds", {}),
    ("GET", "/server/webcams/list", {}),
    ("POST", "/server/webcams/item", {"json": {"name": "cam1", "stream_url": "/webcam/?action=stream"}}),
    ("GET", "/server/webcams/item?name=cam1", {}),
    ("DELETE", "/server/webcams/item?name=cam1", {}),
    ("GET", "/server/extensions/list", {}),
    ("POST", "/server/extensions/request", {"json": {"agent": "x", "method": "y"}}),
    ("GET", "/server/logs/rollover", {}),
    ("POST", "/server/jsonrpc", {"json": {"jsonrpc": "2.0", "method": "server.info", "id": 5}}),
    ("POST", "/server/jsonrpc", {"json": {"jsonrpc": "2.0", "method": "nope", "id": 5}}),
    ("POST", "/server/jsonrpc", {"data": "nojson", "headers": {"Content-Type": "application/json"}}),
    ("POST", "/server/jsonrpc", {"data": "x"}),
    ("GET", "/server/redirect?url=http://evil.com", {}),
    ("OPTIONS", "/server/info", {"headers": {"Origin": "http://my.example.com"}}),
    ("GET", "/server/info", {"headers": {"Origin": "http://my.example.com"}}),
    ("GET", "/api/version", {}),
    ("GET", "/api/server", {}),
    ("GET", "/api/login", {}),
    ("GET", "/api/settings", {}),
    ("GET", "/api/job", {}),
    ("GET", "/api/printer", {}),
    ("GET", "/api/printerprofiles", {}),
    ("GET", "/server/files/moonraker.log", {}),
]

WS_CASES = [
    ("server.info", {}),
    ("server.connection.identify", {"client_name": "difftest", "version": "1.0", "type": "web", "url": "http://x"}),
    ("server.connection.identify", {"client_name": "difftest", "version": "1.0", "type": "web", "url": "http://x"}),
    ("server.websocket.id", {}),
    ("printer.info", {}),
    ("printer.objects.subscribe", {"objects": {"webhooks": None, "toolhead": ["position"]}}),
    ("printer.objects.query", {"objects": {"print_stats": None}}),
    ("printer.gcode.script", {"script": "M115"}),
    ("server.files.list", {"root": "gcodes"}),
    ("server.files.get_directory", {"path": "gcodes"}),
    ("server.database.get_item", {"namespace": "test"}),
    ("server.database.post_item", {"namespace": "test", "key": "ws", "value": {"a": [1, 2]}}),
    ("server.database.delete_item", {"namespace": "test", "key": "ws"}),
    ("machine.system_info", {}),
    ("server.temperature_store", {"include_monitors": True}),
    ("server.history.list", {"limit": 5}),
    ("server.job_queue.status", {}),
    ("server.announcements.list", {}),
    ("server.webcams.list", {}),
    ("access.info", {}),
    ("nonexistent.method", {}),
    ("server.files.metadata", {"filename": "nope"}),
    ("printer.objects.list", {}),
    ("server.config", {}),
]

async def ws_run(host):
    conn = await tornado.websocket.websocket_connect(f"ws://{host}/websocket")
    results = []
    rid = 1
    for method, params in WS_CASES:
        msg = {"jsonrpc": "2.0", "method": method, "params": params, "id": rid}
        await conn.write_message(json.dumps(msg))
        while True:
            resp = await asyncio.wait_for(conn.read_message(), 10)
            data = json.loads(resp)
            if data.get("id") == rid:
                results.append(data)
                break
        rid += 1
    # bad messages
    for raw in ["not json", json.dumps({"jsonrpc": "1.0", "method": "server.info", "id": 99}), json.dumps({"jsonrpc": "2.0", "method": "server.info", "params": [1], "id": 98}), json.dumps([{"jsonrpc": "2.0", "method": "server.info", "id": 97}, {"jsonrpc": "2.0", "method": "nope", "id": 96}])]:
        await conn.write_message(raw)
        while True:
            resp = await asyncio.wait_for(conn.read_message(), 10)
            data = json.loads(resp)
            if isinstance(data, list) or "id" in data:
                results.append(data)
                break
    conn.close()
    return results

def main():
    filt = sys.argv[1] if len(sys.argv) > 1 else ""
    total = 0
    bad = 0
    for method, path, kw in HTTP_CASES:
        if filt and filt not in path:
            continue
        total += 1
        a = norm(http(REF, method, path, **kw))
        b = norm(http(RUST, method, path, **kw))
        d = diff(a, b)
        if d:
            bad += 1
            print(f"=== {method} {path}")
            for line in d[:25]:
                print("   ", line)
    if not filt or filt == "ws":
        ra = asyncio.run(ws_run(REF))
        rb = asyncio.run(ws_run(RUST))
        for i, (x, y) in enumerate(zip(ra, rb)):
            total += 1
            name = WS_CASES[i][0] if i < len(WS_CASES) else f"raw{i}"
            d = diff(norm(x), norm(y))
            if d:
                bad += 1
                print(f"=== WS {name}")
                for line in d[:25]:
                    print("   ", line)
    print(f"\n{total - bad}/{total} cases identical")

if __name__ == "__main__":
    main()
