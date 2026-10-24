#!/usr/bin/env python3
# Python component parity (docs/PLUGINS.md Phase 3+): runs Python Moonraker
# and Moonruster on the same test components (tools/plugin_tests/components,
# symlinked into a clone of the reference tree, as installers do) and the
# same config, then compares /server/info and every log entry about the test
# components. Timestamps, paths and the instance's own values are masked.
#
# usage (WSL, ~/mr setup): ~/mr/venv/bin/python tools/plugin_cmp.py [moonruster binary]
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time
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
    return d


def wait_up(port, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/server/info", timeout=2) as r:
                return json.load(r)["result"]
        except Exception:
            time.sleep(0.3)
    raise SystemExit(f"server on port {port} did not come up")


def run(cmd, env, port, d):
    p = subprocess.Popen(cmd, env=env, cwd=str(TREE), stdout=open(d / "logs/stdout", "w"), stderr=subprocess.STDOUT)
    info = wait_up(port)
    time.sleep(1.0)  # component_init of optional components
    info = wait_up(port)
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(20)
    except subprocess.TimeoutExpired:
        p.kill()
    return info, (d / "logs/moonraker.log").read_text(errors="replace")


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
        # - the server's own lines: Moonruster names the module, not the function
        e = re.sub(r"^\[server\.py:\w+\(\)\]", "[server.py]", e)
        # - the instances' own port
        e = re.sub(r"\bport = 70[45]0\b", "port = <PORT>", e)
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
