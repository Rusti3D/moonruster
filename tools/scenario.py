#!/usr/bin/env python3
# Stateful differential test: runs the same sequence of actions (uploads,
# file ops, prints, history, queue, auth...) against the reference moonraker
# and moonruster, recording each response and the websocket notifications
# that follow it, then reports differences.
#
# usage: scenario.py [step-filter]
import sys
import os
import json
import time
import hashlib
import asyncio
import threading
import requests
import tornado.websocket
from difftest import REF, RUST, norm, diff

HERE = os.path.dirname(os.path.abspath(__file__))
SAMPLES = os.path.join(HERE, "samples")
SKIP_NOTIFY = {"notify_proc_stat_update", "notify_status_update", "notify_gcode_response",
               "notify_sensor_update", "notify_spoolman_status_changed", "notify_active_spool_set"}
EXTRA_VOLATILE = {"job_id", "uid", "last_modified", "permissions", "filament_used",
                  "print_start_time", "user_id", "salt", "jwt_secret"}

PRINT_GCODE = "G28\nG90\nG1 Z5 F600\n" + "".join(
    f"G1 X{20 + i * 5} Y{20 + i * 3} F6000\n" for i in range(40)) + "M400\n"

class Listener:
    def __init__(self, host):
        self.host = host
        self.events = []
        self.lock = threading.Lock()
        self.loop = asyncio.new_event_loop()
        self.ready = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        self.ready.wait(10)

    def _run(self):
        asyncio.set_event_loop(self.loop)
        self.loop.run_until_complete(self._listen())

    async def _listen(self):
        while True:
            try:
                conn = await tornado.websocket.websocket_connect(f"ws://{self.host}/websocket")
                await conn.write_message(json.dumps({
                    "jsonrpc": "2.0", "method": "server.connection.identify", "id": 1,
                    "params": {"client_name": "scenario", "version": "1", "type": "web", "url": "x"}}))
                self.ready.set()
                while True:
                    msg = await conn.read_message()
                    if msg is None:
                        with self.lock:
                            self.events.append(("<ws closed>", None))
                        break
                    data = json.loads(msg)
                    m = data.get("method")
                    if m and m not in SKIP_NOTIFY:
                        with self.lock:
                            self.events.append((m, data.get("params")))
            except Exception:
                await asyncio.sleep(0.5)

    def take(self):
        with self.lock:
            ev, self.events = self.events, []
        return ev

def http(host, method, path, **kw):
    r = requests.request(method, f"http://{host}{path}", timeout=60, **kw)
    ct = r.headers.get("content-type", "")
    if "json" in ct:
        body = r.json()
    elif ct.startswith("text"):
        body = r.text
    else:
        body = {"<bytes>": len(r.content), "sha1": hashlib.sha1(r.content).hexdigest()}
    return {"status": r.status_code, "body": body}

def upload(name, data=None, **fields):
    if data is None:
        with open(os.path.join(SAMPLES, name), "rb") as f:
            data = f.read()
    fname = fields.pop("fname", os.path.basename(name))
    return {"files": {"file": (fname, data)}, "data": fields}

def wait_print_done(host, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        r = requests.get(f"http://{host}/printer/objects/query?print_stats").json()
        st = r["result"]["status"]["print_stats"]["state"]
        if st not in ("printing", "paused"):
            return st
        time.sleep(0.3)
    return "timeout"

def wait_state(host, states, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            r = requests.get(f"http://{host}/printer/objects/query?print_stats").json()
            if r["result"]["status"]["print_stats"]["state"] in states:
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False

def wait_klippy_ready(host, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if requests.get(f"http://{host}/server/info").json()["result"]["klippy_state"] == "ready":
                return True
        except Exception:
            pass
        time.sleep(0.3)
    return False

SAMPLE_NAMES = sorted(n for n in os.listdir(SAMPLES)) if os.path.isdir(SAMPLES) else []

def build_steps():
    S = []
    def step(name, method, path, wait=0.6, after=None, **kw):
        S.append((name, method, path, kw, wait, after))
    # clean slate
    step("clean_dir", "DELETE", "/server/files/directory?path=gcodes/scen&force=true", wait=1.0)
    step("clean_hist", "DELETE", "/server/history/job?all=true")
    step("reset_totals", "POST", "/server/history/reset_totals")
    step("clean_queue", "DELETE", "/server/job_queue/job?all=true")
    step("mkdir", "POST", "/server/files/directory?path=gcodes/scen")
    for n in SAMPLE_NAMES:
        step(f"upload {n}", "POST", "/server/files/upload", wait=1.0, **upload(n, path="scen"))
    for n in SAMPLE_NAMES:
        step(f"metadata {n}", "GET", f"/server/files/metadata?filename=scen/{n}")
    step("thumbs prusa", "GET", "/server/files/thumbnails?filename=scen/prusa.gcode")
    step("thumbs ufp", "GET", "/server/files/thumbnails?filename=scen/cura_pkg.gcode")
    step("thumb file", "GET", "/server/files/gcodes/scen/.thumbs/prusa-16x16.png")
    step("thumb file2", "GET", "/server/files/gcodes/scen/.thumbs/prusa-32x24.png")
    step("dir ext", "GET", "/server/files/directory?path=gcodes/scen&extended=true")
    step("list", "GET", "/server/files/list?root=gcodes")
    step("download", "GET", "/server/files/gcodes/scen/prusa.gcode")
    step("download range", "GET", "/server/files/gcodes/scen/prusa.gcode", headers={"Range": "bytes=10-49"})
    step("download 404", "GET", "/server/files/gcodes/scen/none.gcode")
    step("metascan", "GET", "/server/files/metascan?filename=scen/unknown.gcode", wait=1.0)
    step("copy", "POST", "/server/files/copy", json={"source": "gcodes/scen/prusa.gcode", "dest": "gcodes/scen/copy.gcode"}, wait=1.0)
    step("copy exists", "POST", "/server/files/copy", json={"source": "gcodes/scen/prusa.gcode", "dest": "gcodes/scen/copy.gcode"}, wait=1.0)
    step("move", "POST", "/server/files/move", json={"source": "gcodes/scen/copy.gcode", "dest": "gcodes/scen/moved.gcode"}, wait=1.0)
    step("meta moved", "GET", "/server/files/metadata?filename=scen/moved.gcode")
    step("thumbs moved", "GET", "/server/files/thumbnails?filename=scen/moved.gcode")
    step("mkdir sub", "POST", "/server/files/directory", json={"path": "gcodes/scen/sub"})
    step("copy to sub", "POST", "/server/files/copy", json={"source": "gcodes/scen/cura.gcode", "dest": "gcodes/scen/sub/"}, wait=1.0)
    step("move dir", "POST", "/server/files/move", json={"source": "gcodes/scen/sub", "dest": "gcodes/scen/sub2"}, wait=1.0)
    step("copy dir", "POST", "/server/files/copy", json={"source": "gcodes/scen/sub2", "dest": "gcodes/scen/sub3"}, wait=1.0)
    step("meta in sub", "GET", "/server/files/metadata?filename=scen/sub2/cura.gcode")
    step("move bad root", "POST", "/server/files/move", json={"source": "gcodes/scen/sub2", "dest": "nope/sub2"})
    step("move missing", "POST", "/server/files/move", json={"source": "gcodes/scen/zzz", "dest": "gcodes/scen/yyy"})
    step("zip", "POST", "/server/files/zip", json={"items": ["gcodes/scen/sub2", "gcodes/scen/prusa.gcode"], "dest": "gcodes/scen/arch.zip", "store_only": False}, wait=1.0)
    step("zip default", "POST", "/server/files/zip", json={"items": ["gcodes/scen/kiri.gcode"]}, wait=1.0)
    step("delete file", "DELETE", "/server/files/gcodes/scen/moved.gcode", wait=1.0)
    step("delete missing", "DELETE", "/server/files/gcodes/scen/moved.gcode")
    step("rmdir nonempty", "DELETE", "/server/files/directory?path=gcodes/scen/sub3")
    step("rmdir force", "DELETE", "/server/files/directory?path=gcodes/scen/sub3&force=true", wait=1.0)
    data = open(os.path.join(SAMPLES, "unknown.gcode"), "rb").read()
    step("upload bad checksum", "POST", "/server/files/upload", **upload("unknown.gcode", path="scen", checksum="0" * 64, fname="ck.gcode"))
    step("upload good checksum", "POST", "/server/files/upload", wait=1.0, **upload("unknown.gcode", path="scen", checksum=hashlib.sha256(data).hexdigest(), fname="ck.gcode"))
    step("upload overwrite", "POST", "/server/files/upload", wait=1.0, **upload("unknown.gcode", path="scen", fname="ck.gcode"))
    step("upload config", "POST", "/server/files/upload", wait=1.0, **upload("notes.txt", root="config", fname="scen_test.cfg"))
    step("upload bad root", "POST", "/server/files/upload", **upload("notes.txt", root="nope"))
    step("upload bad ext", "POST", "/server/files/upload", **upload("notes.txt", root="gcodes", path="scen"))
    step("upload no file", "POST", "/server/files/upload", data={"root": "gcodes"})
    step("upload abs path", "POST", "/server/files/upload", **upload("unknown.gcode", path="/scen/../../x"))
    step("del config", "DELETE", "/server/files/config/scen_test.cfg", wait=1.0)
    step("upload print", "POST", "/server/files/upload", wait=0.5, after=("print_done",),
         **upload("p.gcode", data=PRINT_GCODE.encode(), path="scen", print="true"))
    step("history list", "GET", "/server/history/list?limit=10")
    step("history totals", "GET", "/server/history/totals")
    step("print start", "POST", "/printer/print/start?filename=scen/p.gcode", wait=0.2, after=("printing",))
    step("print pause", "POST", "/printer/print/pause", wait=0.5, after=("paused",))
    step("print status paused", "GET", "/printer/objects/query?print_stats=state,filename,info&virtual_sdcard=file_path,is_active&pause_resume")
    step("print resume", "POST", "/printer/print/resume", wait=0.2)
    step("print cancel", "POST", "/printer/print/cancel", wait=0.5, after=("print_done",))
    step("history list2", "GET", "/server/history/list?limit=10&order=asc")
    step("history list since", "GET", "/server/history/list?since=0&before=99999999999&start=1")
    step("history totals2", "GET", "/server/history/totals")
    step("queue add", "POST", "/server/job_queue/job", json={"filenames": ["scen/p.gcode", "scen/unknown.gcode"]})
    step("queue status", "GET", "/server/job_queue/status")
    step("queue add reset", "POST", "/server/job_queue/job?filenames=scen/prusa.gcode&reset=true")
    step("queue add missing", "POST", "/server/job_queue/job?filenames=scen/none.gcode")
    step("queue jump", "POST", "/server/job_queue/jump?job_id=0")
    step("queue pause", "POST", "/server/job_queue/pause")
    step("queue del all", "DELETE", "/server/job_queue/job?all=true")
    step("queue add2", "POST", "/server/job_queue/job?filenames=scen/p.gcode")
    step("queue start", "POST", "/server/job_queue/start", wait=0.5, after=("print_done",))
    step("queue status2", "GET", "/server/job_queue/status", wait=1.0)
    step("history list3", "GET", "/server/history/list?limit=3")
    step("history del bad", "DELETE", "/server/history/job?uid=zzzz")
    step("gcode store", "GET", "/server/gcode_store?count=5")
    # auth
    step("user create", "POST", "/access/user", json={"username": "scenuser", "password": "Scen-pass1"})
    step("user create dup", "POST", "/access/user", json={"username": "scenuser", "password": "Scen-pass1"})
    step("users list", "GET", "/access/users/list")
    step("login bad", "POST", "/access/login", json={"username": "scenuser", "password": "wrong"})
    step("login", "POST", "/access/login", json={"username": "scenuser", "password": "Scen-pass1", "source": "moonraker"}, after=("save_token",))
    step("user with jwt", "GET", "/access/user", after=("auth",))
    step("refresh", "POST", "/access/refresh_jwt", after=("refresh",))
    step("pw reset bad", "POST", "/access/user/password", json={"password": "wrong", "new_password": "x2-Scen-pass"}, after=("auth",))
    step("pw reset", "POST", "/access/user/password", json={"password": "Scen-pass1", "new_password": "x2-Scen-pass"}, after=("auth",))
    step("oneshot", "GET", "/access/oneshot_token", after=("auth",))
    step("logout", "POST", "/access/logout", after=("auth",))
    step("user after logout", "GET", "/access/user", after=("auth",))
    step("user delete", "DELETE", "/access/user", json={"username": "scenuser"})
    step("user delete missing", "DELETE", "/access/user", json={"username": "scenuser"})
    step("bad jwt", "GET", "/server/info", headers={"Authorization": "Bearer abc.def.ghi"})
    step("api key get", "GET", "/access/api_key")
    step("api key use", "GET", "/server/info", after=("apikey",))
    step("bad api key", "GET", "/server/info", headers={"X-Api-Key": "nope"})
    # misc
    step("announce dismiss", "POST", "/server/announcements/dismiss?entry_id=nope")
    step("announce feed add", "POST", "/server/announcements/feeds?name=testfeed")
    step("announce feed del", "DELETE", "/server/announcements/feeds?name=testfeed")
    step("services restart", "POST", "/machine/services/restart?service=klipper")
    step("octo upload", "POST", "/api/files/local", wait=1.0, **upload("unknown.gcode", fname="octo.gcode", path="scen"))
    step("octo upload print", "POST", "/api/files/local", wait=0.5, after=("print_done",), **upload("p.gcode", data=PRINT_GCODE.encode(), fname="octo_p.gcode", print="true"))
    step("webcam add", "POST", "/server/webcams/item", json={"name": "scam", "service": "mjpegstreamer", "stream_url": "/webcam/?action=stream", "snapshot_url": "/webcam/?action=snapshot", "rotation": 90, "flip_horizontal": True, "extra_data": {"x": 1}})
    step("webcam update", "POST", "/server/webcams/item", json={"name": "scam", "target_fps": 30})
    step("webcam bad rot", "POST", "/server/webcams/item", json={"name": "scam2", "stream_url": "/x", "rotation": 45})
    step("webcam list", "GET", "/server/webcams/list")
    step("webcam test", "POST", "/server/webcams/test?name=scam")
    step("webcam del", "DELETE", "/server/webcams/item?name=scam")
    step("data store", "GET", "/server/temperature_store?include_monitors=true")
    step("gcode script", "POST", "/printer/gcode/script", json={"script": "M118 hello scen\nRESPOND MSG=\"x y\""})
    # emergency_stop/firmware_restart are left out: batch mode klippy hangs after them
    step("restart klippy", "POST", "/printer/restart", wait=0.5, after=("ready",))
    step("final dir", "GET", "/server/files/directory?path=gcodes/scen")
    step("final clean", "DELETE", "/server/files/directory?path=gcodes/scen&force=true", wait=1.0)
    return S

def run(host, steps):
    lis = Listener(host)
    time.sleep(0.5)
    lis.take()
    out = []
    token = {}
    for name, method, path, kw, wait, after in steps:
        kw = dict(kw)
        mode = after[0] if after else None
        if mode == "auth" and token.get("jwt"):
            kw["headers"] = {"Authorization": f"Bearer {token['jwt']}"}
        if mode == "refresh":
            kw["json"] = {"refresh_token": token.get("refresh")}
        if mode == "apikey":
            kw["headers"] = {"X-Api-Key": token.get("apikey", "")}
        if "files" in kw:
            # requests consumes nothing, but rebuild tuples each run
            kw["files"] = {k: (v[0], v[1]) for k, v in kw["files"].items()}
        try:
            resp = http(host, method, path, **kw)
        except Exception as e:
            resp = {"error": str(e)}
        if name == "api key get":
            token["apikey"] = resp.get("body", {}).get("result", "")
        if mode == "save_token":
            res = resp.get("body", {}).get("result", {})
            token["jwt"] = res.get("token")
            token["refresh"] = res.get("refresh_token")
        if mode == "refresh":
            res = resp.get("body", {}).get("result", {})
            if res.get("token"):
                token["jwt"] = res["token"]
        time.sleep(wait)
        if mode == "print_done":
            wait_state(host, ("printing",), 10)
            resp["final_state"] = wait_print_done(host)
            time.sleep(1.5)
        elif mode == "printing":
            resp["reached"] = wait_state(host, ("printing",))
        elif mode == "paused":
            resp["reached"] = wait_state(host, ("paused",))
        elif mode == "ready":
            resp["reached"] = wait_klippy_ready(host)
            time.sleep(1.5)
        out.append((name, resp, lis.take()))
    return out

def normalize_events(evs):
    res = []
    for m, p in evs:
        res.append({"method": m, "params": p})
    return res

def nrm(v, key=None):
    if key in EXTRA_VOLATILE:
        return "<volatile>"
    if isinstance(v, dict):
        return {k: nrm(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [nrm(x, key) for x in v]
    return v

def main():
    filt = sys.argv[1] if len(sys.argv) > 1 else ""
    steps = [s for s in build_steps() if filt in s[0]] if filt else build_steps()
    ra = run(REF, steps)
    rb = run(RUST, steps)
    bad = 0
    for (name, a, ea), (_, b, eb) in zip(ra, rb):
        d = diff(nrm(norm(a)), nrm(norm(b)), "$resp")
        d += diff(nrm(norm(normalize_events(ea))), nrm(norm(normalize_events(eb))), "$notify")
        if d:
            bad += 1
            print(f"=== {name}")
            for line in d[:30]:
                print("   ", line)
    print(f"\n{len(steps) - bad}/{len(steps)} steps identical")

if __name__ == "__main__":
    main()
