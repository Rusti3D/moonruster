#!/usr/bin/env python3
# Python component parity (docs/PLUGINS.md Phase 3+): runs Python Moonraker
# and Moonruster on the same test components (tools/plugin_tests/components,
# symlinked into a clone of the reference tree, as installers do) and the
# same config, then compares /server/info and every log entry about the test
# components. Timestamps, paths and the instance's own values are masked.
#
# usage (WSL, ~/mr setup): ~/mr/venv/bin/python tools/plugin_cmp.py [moonruster binary]
import asyncio
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

HOME = pathlib.Path.home()
HERE = pathlib.Path(__file__).resolve().parent
TREE = HOME / "mr/ptest/moonraker"
VENV_PY = HOME / "mr/venv/bin/python"
RUST = sys.argv[1] if len(sys.argv) > 1 else str(HOME / "mr/target/release/moonruster")
CONF = """[server]
host: 127.0.0.1
port: {port}
klippy_uds_address: {d}/comms/klippy.sock

[authorization]
trusted_clients:
    127.0.0.0/8

[machine]
provider: none
validate_service: False

[mr_test_basic]
str_opt: hello
int_opt: 5
float_opt: 1.5
bool_opt: true

[mr_test_fail_import]

[mr_test_fail_init]

[mr_test_source_info]

[mr_test_endpoints]

[mr_test_events]

[mr_test_database]

[mr_test_klippy]

[mr_test_shell]

[mqtt]
address: localhost
instance_name: mr_test_cmp

[mr_test_mqtt]

[mr_test_services]

[mr_test_config]
a_list:
  one
  two
csv: a, b, c
ints: 1, 2, 3
floats: 1.5, 2
matrix:
  1, 2
  3, 4
a_dict:
  k1 = v1
  k2=v2
int_dict:
  a = 1
  b = 2
bad_dict:
  good = 1
  lonely
mode: fast
tmpl: value {{ x * 2 }}
a_path: ~/mr_test_dir/../sub
flag: yes
"""

# a batch Klippy behind each server (tools/start_klippy.sh), with a macro
# that calls the test component's remote method
REMOTE_MACRO = """
[gcode_macro MR_TEST_REMOTE]
gcode:
  {action_call_remote_method("mr_test_remote", a=1, b="two")}
"""


def setup_tree():
    if not TREE.exists():
        TREE.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", "-q", str(HOME / "mr/moonraker"), str(TREE)], check=True)
    subprocess.run(["git", "-C", str(TREE), "checkout", "-q", "9e676eba"], check=True)
    comps = TREE / "moonraker/components"
    for f in (HERE / "plugin_tests/components").glob("*.py"):
        link = comps / f.name
        if link.is_symlink() or link.exists():
            link.unlink()
        link.symlink_to(f)


def setup_instance(name, port):
    d = HOME / "mr/inst" / name
    if d.exists():
        shutil.rmtree(d)
    for sub in ("config", "logs", "comms", "database", "gcodes"):
        (d / sub).mkdir(parents=True)
    (d / "config/moonraker.conf").write_text(CONF.format(port=port, d=d))
    src = (HOME / "mr/moonraker/tests/assets/klipper/base_printer.cfg").read_text()
    keys = {"gcode_path": f"{d}/gcodes", "klippy_uds_path": f"{d}/comms/klippy.sock"}
    cfg = re.sub(r"\$\{([^}]+)\}", lambda m: keys.get(m.group(1), m.group(0)), src)
    cfg = "\n".join(l for l in cfg.splitlines() if not l.startswith("relative_reference_index")) + "\n"
    (d / "config/printer.cfg").write_text(cfg + REMOTE_MACRO)
    return d


def start_klippy(d):
    return subprocess.Popen(
        [str(VENV_PY), "klippy.py", str(d / "config/printer.cfg"), "-o", "/dev/null", "-d", str(HOME / "mr/klipper.dict"),
         "-a", str(d / "comms/klippy.sock"), "-I", str(d / "comms/klippy_pty"), "-l", str(d / "logs/klippy.log")],
        cwd=str(HOME / "mr/klipper/klippy"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def wait_klippy_ready(port, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        if wait_up(port).get("klippy_state") == "ready":
            return
        time.sleep(0.2)
    raise SystemExit(f"klippy behind port {port} did not get ready")


def read_log(d):
    """the run's whole log: a run across midnight is split by the daily
    rollover into moonraker.log.<date> and moonraker.log"""
    logs = sorted((d / "logs").glob("moonraker.log.*")) + [d / "logs/moonraker.log"]
    return "".join(p.read_text(errors="replace") for p in logs if p.exists())


def port_free(port):
    """a server left over from an earlier run would answer instead of the
    one started now: refuse to compare then"""
    import socket
    with socket.socket() as s:
        if s.connect_ex(("127.0.0.1", port)) == 0:
            raise SystemExit(f"port {port} is in use (a server from an earlier run?)")


def wait_up(port, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/server/info", timeout=2) as r:
                return json.load(r)["result"]
        except Exception:
            time.sleep(0.3)
    raise SystemExit(f"server on port {port} did not come up")


# requests answered by the test components: (method, path, json body)
REQUESTS = [
    ("GET", "/server/mr_test/info", None),
    ("GET", "/server/mr_test/info?a=1&b=two", None),
    ("GET", "/server/mr_test/args?s=x&i=5&f=2.5&b=true&l=a,b", None),
    ("POST", "/server/mr_test/args", {"s": "y", "i": "7", "l": ["q"]}),
    ("GET", "/server/mr_test/raw", None),
    ("GET", "/server/mr_test/notfound", None),
    ("GET", "/server/mr_test/valueerror", None),
    ("GET", "/server/mr_test/typeerror", None),
    ("GET", "/server/mr_test/notasync", None),
    ("POST", "/server/mr_test/none", None),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.info", "params": {"x": 1}, "id": 1}),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.args", "params": {"i": "nan?"}, "id": 2}),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.notfound", "id": 3}),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.valueerror", "id": 4}),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.typeerror", "id": 5}),
    ("GET", "/server/mr_test/removable", None),
    ("POST", "/server/mr_test/app", None),
    ("GET", "/server/mr_test/removable", None),
    ("POST", "/server/jsonrpc", {"jsonrpc": "2.0", "method": "server.mr_test.removable", "id": 6}),
    ("GET", "/server/mr_test/static", None),
    ("POST", "/server/mr_test/db", None),
    ("POST", "/server/mr_test/klippy", None),
    ("POST", "/server/mr_test/shell", None),
    ("POST", "/server/mr_test/services", None),
    ("POST", "/server/mr_test/mqtt", None),
]


def call(port, method, path, body):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            status, ctype, raw = r.status, r.headers.get("Content-Type"), r.read()
    except urllib.error.HTTPError as e:
        status, ctype, raw = e.code, e.headers.get("Content-Type"), e.read()
    try:
        content = json.loads(raw)
    except ValueError:
        content = raw.decode(errors="replace")
    return {"status": status, "content_type": ctype, "body": content}


NOTIFICATIONS = ("notify_notify", "notify_mr_test_named", "notify_mr_test_direct", "notify_mr_test_all",
                 "notify_mr_test_masked")


async def ws_session(port):
    """a websocket client: identify, fire the test component's events, keep
    the notifications it registered, close (client_* events)"""
    from tornado.websocket import websocket_connect
    ws = await websocket_connect(f"ws://127.0.0.1:{port}/websocket")
    out = []

    async def read_until(id, extra=0.0):
        end = None
        while True:
            timeout = 10 if end is None else max(end - time.time(), 0.01)
            try:
                m = await asyncio.wait_for(ws.read_message(), timeout)
            except asyncio.TimeoutError:
                return
            d = json.loads(m)
            if d.get("method") in NOTIFICATIONS:
                out.append(d)
            elif d.get("id") == id:
                d.get("result", {}).pop("connection_id", None) if isinstance(d.get("result"), dict) else None
                out.append(d)
                if not extra:
                    return
                end = time.time() + extra

    def rpc(method, params, id):
        ws.write_message(json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": id}))

    rpc("server.connection.identify", {"client_name": "mr_test", "version": "1", "type": "web", "url": "x"}, 1)
    await read_until(1)
    rpc("server.mr_test.fire", {}, 2)
    await read_until(2, extra=0.5)
    ws.close()
    await asyncio.sleep(0.5)
    return out


def run(cmd, env, port, d):
    port_free(port)
    k = start_klippy(d)
    p = subprocess.Popen(cmd, env=env, cwd=str(TREE), stdout=open(d / "logs/stdout", "w"), stderr=subprocess.STDOUT)
    info = wait_up(port)
    wait_klippy_ready(port)
    time.sleep(1.0)  # component_init of optional components
    info = wait_up(port)
    responses = [call(port, m, path, b) for m, path, b in REQUESTS]
    responses.append({"websocket": asyncio.run(ws_session(port))})
    # a Rust-sent event with a value (file_manager:filelist_changed)
    call(port, "POST", "/server/files/directory?path=gcodes/mr_test_dir", None)
    time.sleep(0.3)
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(20)
    except subprocess.TimeoutExpired:
        p.kill()
    k.kill()
    k.wait()
    return info, read_log(d), responses


def log_entries(text):
    """log entries (a line plus its continuation lines) about the test components"""
    entries, cur = [], None
    for line in text.splitlines():
        if re.match(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d{3} ", line):
            if cur:
                entries.append(cur)
            cur = line[24:]
        elif cur is not None:
            cur += "\n" + line
    if cur:
        entries.append(cur)
    out = []
    for e in (e for e in entries if "mr_test" in e):
        e = re.sub(r"/root/mr/inst/p(ref|rust)", "<D>", re.sub(r'File "[^"]*/', 'File "', e))
        # known differences, not of the component layer (docs/PLUGINS.md):
        # - the server's own lines: Moonruster names the module, not the
        #   function; request durations vary
        e = re.sub(r"^\[(?!mr_test)(\w+\.py):\w+\(\)\]", r"[\1]", e)
        e = re.sub(r"\b\d+\.\d+ms\b", "<ms>", e)
        # - object addresses in reprs
        e = re.sub(r" at 0x[0-9a-f]+>", " at 0x?>", e)
        # - connection IDs (id(self) in Moonraker)
        e = re.sub(r"\bWebsocket \d+\b", "Websocket <ID>", e)
        # - the instances' own port
        e = re.sub(r"\bport = 70[45]0\b", "port = <PORT>", e)
        e = re.sub(r"127\.0\.0\.1:70[45]0", "127.0.0.1:<PORT>", e)
        # - traceback frames of the caller (Moonraker's server.py and importlib
        #   in Python; Rust in Moonruster): only the component's own frames
        lines, keep, skip = e.split("\n"), [], False
        for line in lines:
            if line.startswith('  File "'):
                skip = 'File "mr_test' not in line
                if skip:
                    continue
            elif skip and line.startswith("    "):
                continue
            else:
                skip = False
            keep.append(line)
        out.append("\n".join(keep))
    # Moonraker loads optional components in Python set order (random)
    return sorted(out)


def main():
    setup_tree()
    dref = setup_instance("pref", 7040)
    drust = setup_instance("prust", 7050)
    env = dict(os.environ)
    ref = run([str(VENV_PY), "moonraker/moonraker.py", "-d", str(dref), "-l", str(dref / "logs/moonraker.log")], env, 7040, dref)
    renv = dict(env, MOONRUSTER_MOONRAKER_DIR=str(TREE), MOONRUSTER_PYTHON=str(VENV_PY))
    rust = run([RUST, "-d", str(drust), "-l", str(drust / "logs/moonraker.log")], renv, 7050, drust)
    fails = 0
    for key in ("failed_components", "warnings"):
        a, b = ref[0][key], rust[0][key]
        a = [re.sub(r"/root/mr/inst/pref", "<D>", x) for x in a] if key == "warnings" else a
        b = [re.sub(r"/root/mr/inst/prust", "<D>", x) for x in b] if key == "warnings" else b
        same = sorted(a) == sorted(b)
        fails += not same
        print(f"{'SAME' if same else 'DIFF'} server.info.{key}" + ("" if same else f"\n  ref:  {a}\n  rust: {b}"))
    a, b = sorted(c for c in ref[0]["components"] if c.startswith("mr_test")), sorted(c for c in rust[0]["components"] if c.startswith("mr_test"))
    fails += a != b
    print(f"{'SAME' if a == b else 'DIFF'} server.info.components (test ones)" + ("" if a == b else f"\n  ref:  {a}\n  rust: {b}"))
    for (m, path, b), ra, rb in zip(REQUESTS + [("WS", "/websocket", None)], ref[2], rust[2]):
        # known difference: Moonruster's error bodies carry a synthetic
        # traceback (STATUS.md), for Rust and Python callbacks alike
        for r in (ra, rb):
            if isinstance(r.get("body"), dict) and isinstance(r["body"].get("error"), dict):
                r["body"]["error"].pop("traceback", None)
        same = ra == rb
        fails += not same
        label = f"{m} {path}" + (f" {b['method']}" if isinstance(b, dict) and "method" in b else "")
        print(f"{'SAME' if same else 'DIFF'} {label}: {ra.get('status', '')}" + ("" if same else f"\n  ref:  {ra}\n  rust: {rb}"))
    la, lb = log_entries(ref[1]), log_entries(rust[1])
    import difflib
    diff = list(difflib.unified_diff(la, lb, "ref", "rust", lineterm="", n=1))
    fails += bool(diff)
    print(f"{'SAME' if not diff else 'DIFF'} log entries about the test components ({len(la)} ref, {len(lb)} rust)")
    for line in diff:
        print("  " + line)
    print(f"RESULT {fails} differences")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
