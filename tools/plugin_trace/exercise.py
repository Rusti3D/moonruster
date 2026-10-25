#!/usr/bin/env python3
# Drives the trace instance (run_trace.sh) through what its components react
# to, so tracer.py records the paths they really use:
#  - every endpoint a component registered (GET, POST, DELETE without args)
#  - every remote method, through a klipper macro per method
#  - uploads (metadata extraction), prints (complete, pause/resume, cancel),
#    a klipper restart, and a clean server shutdown (on_exit / close)
# Endpoints and remote methods are read from the trace itself.
# usage: exercise.py [--no-shutdown]
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import requests

D = pathlib.Path.home() / "mr/inst/trace"
URL = "http://localhost:7030"
TRACE = D / "logs/trace.jsonl"
SAMPLES = pathlib.Path(__file__).resolve().parent.parent / "samples"
MACRO_MARK = "# --- trace remote method macros ---"


def registered():
    eps, rms = set(), set()
    for line in open(TRACE):
        r = json.loads(line)
        if r["kind"] == "call" and r["args"]:
            if r["name"] == "register_endpoint":
                eps.add(r["args"][0])
            elif r["name"] == "register_remote_method":
                rms.add(r["args"][0])
    return sorted(eps), sorted(rms)


def req(method, path, **kw):
    try:
        r = requests.request(method, URL + path, timeout=30, **kw)
        return r.status_code
    except requests.RequestException as e:
        return type(e).__name__


def gcode(script):
    return req("POST", "/printer/gcode/script", params={"script": script})


def state():
    try:
        return requests.get(URL + "/printer/objects/query?print_stats", timeout=5).json()[
            "result"]["status"]["print_stats"]["state"]
    except Exception:
        return None


def wait(pred, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.5)
    return False


def klippy_ready():
    try:
        return requests.get(URL + "/server/info", timeout=5).json()["result"]["klippy_state"] == "ready"
    except Exception:
        return False


def add_macros(rms):
    cfg = D / "config/printer.cfg"
    text = cfg.read_text()
    if MACRO_MARK in text:
        text = text[: text.index(MACRO_MARK)]
    text += MACRO_MARK + "\n"
    for i, name in enumerate(rms):
        text += f"[gcode_macro TRACE_RM_{i}]\ngcode:\n  {{action_call_remote_method(\"{name}\")}}\n\n"
    cfg.write_text(text)
    req("POST", "/printer/restart")
    time.sleep(1)
    wait(klippy_ready)


def upload(path, name=None):
    with open(path, "rb") as f:
        return req("POST", "/server/files/upload", files={"file": (name or path.name, f)})


def main():
    eps, rms = registered()
    print(f"{len(eps)} endpoints, {len(rms)} remote methods")
    for p in eps:
        for m in ("GET", "POST", "DELETE"):
            print(m, p, req(m, p))
    add_macros(rms)
    for i, name in enumerate(rms):
        print("remote", name, gcode(f"TRACE_RM_{i}"))
    for s in ("prusa.gcode", "superslicer.gcode", "orca_small.gcode", "cura.gcode"):
        print("upload", s, upload(SAMPLES / s))
    time.sleep(3)  # metadata extraction
    print("print", req("POST", "/printer/print/start", params={"filename": "orca_small.gcode"}))
    wait(lambda: state() == "printing", 20)
    print("pause", req("POST", "/printer/print/pause"))
    wait(lambda: state() == "paused", 20)
    print("resume", req("POST", "/printer/print/resume"))
    wait(lambda: state() in ("complete", "standby", "error"), 120)
    print("print 2", req("POST", "/printer/print/start", params={"filename": "prusa.gcode"}))
    wait(lambda: state() == "printing", 20)
    print("cancel", req("POST", "/printer/print/cancel"))
    wait(lambda: state() in ("cancelled", "standby"), 30)
    print("history", req("GET", "/server/history/list"))
    print("restart klipper", req("POST", "/printer/restart"))
    time.sleep(1)
    wait(klippy_ready)
    if "--no-shutdown" not in sys.argv:
        subprocess.run(["pkill", "-TERM", "-f", "tracer.py " + str(pathlib.Path.home() / "mr/trace/moonraker")])
        time.sleep(3)
    print("done")


if __name__ == "__main__":
    main()
