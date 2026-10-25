#!/usr/bin/env python3
# Corpus parity (docs/PLUGINS.md Phase 5): Python Moonraker and Moonruster on
# the real third-party components of the trace tree (plugin_trace/
# setup_trace.sh: timelapse, Happy Hare, helix, filaman, uboe x3, geeks3d),
# each server with its own batch Klippy. Both are driven the same way:
# every endpoint and remote method the Phase 0 trace recorded, uploads, a
# print with pause / resume, a cancelled print, a Klipper restart, shutdown.
# Compared: /server/info, each step's status and body, and the log entries
# the components wrote. Timestamps, paths, ports and durations are masked.
#
# usage (WSL, ~/mr setup, after plugin_trace/setup_trace.sh):
#   ~/mr/venv/bin/python tools/corpus_cmp.py [moonruster binary]
import difflib
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time

import requests

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import plugin_cmp as pc  # noqa: E402

HOME = pathlib.Path.home()
HERE = pathlib.Path(__file__).resolve().parent
TREE = HOME / "mr/trace/moonraker"
TRACE_CONF = HOME / "mr/inst/trace/config/moonraker.conf"
SAMPLES = HERE / "samples"
RUST = sys.argv[1] if len(sys.argv) > 1 else str(HOME / "mr/target/release/moonruster")
CORPUS = ("timelapse", "mmu_server", "uboe_metadata", "additional_pre_print_checks", "spool_change_estimator",
          "helix_print", "filaman", "geeks3d")

# what the corpus registered in the Phase 0 trace (plugin_trace/surface.md)
ENDPOINTS = [
    "/machine/timelapse/lastframeinfo", "/machine/timelapse/render", "/machine/timelapse/saveframes",
    "/machine/timelapse/settings", "/server/geeks3d/push_token", "/server/geeks3d/refresh_push_token",
    "/server/geeks3d/status", "/server/geeks3d/test_push_token", "/server/geeks3d/version",
    "/server/helix/3mf_conversions", "/server/helix/print_modified", "/server/helix/status",
    "/server/spoolman/proxy", "/server/spoolman/spool_id", "/server/spoolman/status",
    # the filaman variant (setup_trace.sh filaman)
    "/server/filaman/proxy", "/server/filaman/repush", "/server/filaman/spool_id", "/server/filaman/spool_ids",
    "/server/filaman/status", "/server/spoolman/repush", "/server/spoolman/spool_ids",
]
REMOTE_METHODS = [
    "mmu_td1_request", "moonraker_cleanup_lane_data", "moonraker_push_lane_data", "pre_print_checks",
    "spoolman_clear_spools_for_printer", "spoolman_display_spool_location", "spoolman_get_filaments",
    "spoolman_get_spool_by_uid", "spoolman_get_spool_info", "spoolman_pull_gate_map", "spoolman_push_gate_map",
    "spoolman_refresh", "spoolman_set_active_spool", "spoolman_set_spool_gate", "spoolman_set_spool_uid",
    "spoolman_unset_spool_gate", "timelapse_newframe", "timelapse_render", "timelapse_saveFrames",
    "uboe_spool_change_estimate", "filaman_set_active_spool",
]


def setup_instance(name, port):
    d = HOME / "mr/inst" / name
    if d.exists():
        shutil.rmtree(d)
    for sub in ("config", "logs", "comms", "database", "gcodes", "timelapse", "timelapse_frames"):
        (d / sub).mkdir(parents=True)
    conf = TRACE_CONF.read_text().replace(str(HOME / "mr/inst/trace"), str(d))
    conf = re.sub(r"(?m)^port: \d+", f"port: {port}", conf)
    (d / "config/moonraker.conf").write_text(conf)
    # printer.cfg as plugin_cmp builds it, plus a macro per remote method
    src = (HOME / "mr/moonraker/tests/assets/klipper/base_printer.cfg").read_text()
    keys = {"gcode_path": f"{d}/gcodes", "klippy_uds_path": f"{d}/comms/klippy.sock"}
    cfg = re.sub(r"\$\{([^}]+)\}", lambda m: keys.get(m.group(1), m.group(0)), src)
    cfg = "\n".join(l for l in cfg.splitlines() if not l.startswith("relative_reference_index")) + "\n"
    for i, rm in enumerate(REMOTE_METHODS):
        cfg += f"\n[gcode_macro CORPUS_RM_{i}]\ngcode:\n  {{action_call_remote_method(\"{rm}\")}}\n"
    (d / "config/printer.cfg").write_text(cfg)
    return d


def drive(port):
    url = f"http://127.0.0.1:{port}"
    steps = []

    def req(label, method, path, **kw):
        try:
            r = requests.request(method, url + path, timeout=60, **kw)
            try:
                body = r.json()
            except ValueError:
                body = r.text[:200]
            steps.append([label, r.status_code, body])
        except requests.RequestException as e:
            steps.append([label, type(e).__name__, None])

    def state():
        try:
            return requests.get(url + "/printer/objects/query?print_stats", timeout=5).json()[
                "result"]["status"]["print_stats"]["state"]
        except Exception:
            return None

    def wait(pred, timeout=60):
        end = time.time() + timeout
        while time.time() < end and not pred():
            time.sleep(0.5)

    for p in ENDPOINTS:
        for m in ("GET", "POST", "DELETE"):
            req(f"{m} {p}", m, p)
    for i, rm in enumerate(REMOTE_METHODS):
        req(f"remote {rm}", "POST", "/printer/gcode/script", params={"script": f"CORPUS_RM_{i}"})
    for s in ("prusa.gcode", "superslicer.gcode", "orca_small.gcode", "cura.gcode"):
        with open(SAMPLES / s, "rb") as f:
            req(f"upload {s}", "POST", "/server/files/upload", files={"file": (s, f)})
    time.sleep(3)  # metadata extraction
    req("print", "POST", "/printer/print/start", params={"filename": "orca_small.gcode"})
    wait(lambda: state() == "printing", 20)
    req("pause", "POST", "/printer/print/pause")
    wait(lambda: state() == "paused", 20)
    req("resume", "POST", "/printer/print/resume")
    wait(lambda: state() in ("complete", "standby", "error"), 120)
    req("print 2", "POST", "/printer/print/start", params={"filename": "prusa.gcode"})
    wait(lambda: state() == "printing", 20)
    req("cancel", "POST", "/printer/print/cancel")
    wait(lambda: state() in ("cancelled", "standby"), 30)
    req("restart klipper", "POST", "/printer/restart")
    time.sleep(1)
    pc.wait_klippy_ready(port, 60)
    return steps


def run(cmd, env, port, d):
    pc.port_free(port)
    k = pc.start_klippy(d)
    p = subprocess.Popen(cmd, env=env, cwd=str(TREE), stdout=open(d / "logs/stdout", "w"), stderr=subprocess.STDOUT)
    pc.wait_up(port)
    pc.wait_klippy_ready(port, 60)
    time.sleep(2)
    info = pc.wait_up(port)
    try:
        steps = drive(port)
    finally:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(30)
        except subprocess.TimeoutExpired:
            p.kill()
        k.kill()
        k.wait()
    return info, pc.read_log(d), steps


def mask(s, name):
    s = s.replace(str(HOME / "mr/inst" / name), "<D>")
    s = re.sub(r"\b70[67]0\b", "<PORT>", s)
    s = re.sub(r" at 0x[0-9a-f]+>", " at 0x?>", s)
    s = re.sub(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "<UUID>", s)
    s = re.sub(r"\b\d+\.\d+ms\b", "<ms>", s)
    s = re.sub(r"\b1[0-9]{9}(\.\d+)?\b", "<TS>", s)  # unix times
    s = re.sub(r"\d{4}-\d\d-\d\d[ T_]\d\d[:\-]\d\d[:\-]\d\d", "<DATE>", s)
    return s


def component_entries(text, name):
    """log entries (with continuation lines) written by the corpus components"""
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
    own = tuple(f"[{c}.py:" for c in CORPUS)
    out = []
    for e in entries:
        if not (e.startswith(own) or any(f"({c})" in e for c in CORPUS)):
            continue
        # known difference: the server's own lines name the module, not the
        # function, in Moonruster
        if not e.startswith(own):
            e = re.sub(r"^\[(\w+\.py):\w+\(\)\]", r"[\1]", e)
        out.append(mask(own_frames(e), name))
    return sorted(out)


def own_frames(e):
    """known difference (docs/PLUGINS.md): below a method Moonruster
    implements in Rust (klippy_apis._send_klippy_request), Moonraker's
    traceback continues into its own Python internals; only the components'
    frames are compared"""
    own = tuple(f"/{c}.py\"" for c in CORPUS)
    keep, skip = [], False
    for line in e.split("\n"):
        if line.startswith('  File "'):
            skip = not any(o in line for o in own)
            if skip:
                continue
        elif skip and line.startswith("    "):
            continue
        else:
            skip = False
        keep.append(line)
    return "\n".join(keep)


def load_order(log):
    """the optional components in the order Moonraker loaded them (its
    set order, random per run)"""
    order = []
    for m in re.finditer(r"(?:Component \((\w+)\) loaded|Unable to load component: \((\w+)\))", log):
        name = m.group(1) or m.group(2)
        if name not in order:
            order.append(name)
    return order


def reorder_sections(conf_path, order):
    """Moonruster loads optional components in config order: its config gets
    the sections in the order Moonraker loaded them, so both load alike"""
    blocks, cur = [], []
    for line in conf_path.read_text().splitlines(keepends=True):
        if line.startswith("[") and cur:
            blocks.append(cur)
            cur = []
        cur.append(line)
    if cur:
        blocks.append(cur)

    def key(block):
        name = block[0].strip().strip("[]").split()[0] if block[0].startswith("[") else ""
        return (order.index(name) if name in order else -1)
    blocks.sort(key=key)  # stable: sections Moonraker did not name keep their places, first
    text = "".join("".join(b) for b in blocks)
    conf_path.write_text(text if text.endswith("\n") else text + "\n")


def main():
    if not TREE.exists() or not TRACE_CONF.exists():
        raise SystemExit("run tools/plugin_trace/setup_trace.sh first")
    dref = setup_instance("cref", 7060)
    drust = setup_instance("crust", 7070)
    env = dict(os.environ)
    ref = run([str(pc.VENV_PY), "moonraker/moonraker.py", "-d", str(dref), "-l", str(dref / "logs/moonraker.log")],
              env, 7060, dref)
    reorder_sections(drust / "config/moonraker.conf", load_order(ref[1]))
    renv = dict(env, MOONRUSTER_MOONRAKER_DIR=str(TREE), MOONRUSTER_PYTHON=str(pc.VENV_PY))
    rust = run([RUST, "-d", str(drust), "-l", str(drust / "logs/moonraker.log")], renv, 7070, drust)
    fails = 0
    for key in ("failed_components", "warnings"):
        a = sorted(mask(json.dumps(x), "cref") for x in ref[0][key])
        b = sorted(mask(json.dumps(x), "crust") for x in rust[0][key])
        fails += a != b
        print(f"{'SAME' if a == b else 'DIFF'} server.info.{key}" + ("" if a == b else f"\n  ref:  {a}\n  rust: {b}"))
    a = sorted(c for c in ref[0]["components"] if c in CORPUS)
    b = sorted(c for c in rust[0]["components"] if c in CORPUS)
    fails += a != b
    print(f"{'SAME' if a == b else 'DIFF'} corpus components loaded" + ("" if a == b else f"\n  ref:  {a}\n  rust: {b}"))
    for (label, sa, ba), (_, sb, bb) in zip(ref[2], rust[2]):
        # known difference: Moonruster's error bodies carry a synthetic
        # traceback (STATUS.md)
        for b in (ba, bb):
            if isinstance(b, dict) and isinstance(b.get("error"), dict):
                b["error"].pop("traceback", None)
        ja, jb = mask(json.dumps(ba, sort_keys=True), "cref"), mask(json.dumps(bb, sort_keys=True), "crust")
        same = sa == sb and ja == jb
        fails += not same
        print(f"{'SAME' if same else 'DIFF'} {label}: {sa}" + ("" if same else f"\n  ref:  {sa} {ja[:400]}\n  rust: {sb} {jb[:400]}"))
    la, lb = component_entries(ref[1], "cref"), component_entries(rust[1], "crust")
    diff = list(difflib.unified_diff(la, lb, "ref", "rust", lineterm="", n=0))
    fails += bool(diff)
    print(f"{'SAME' if not diff else 'DIFF'} component log entries ({len(la)} ref, {len(lb)} rust)")
    for line in diff:
        print("  " + line[:600])
    print(f"RESULT {fails} differences")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
