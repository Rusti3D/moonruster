#!/usr/bin/env python3
# Resource gate with Python components (docs/PLUGINS.md Phase 6): Python
# Moonraker and Moonruster on the same instance setup as corpus_cmp.py (the
# real corpus components, a batch Klippy each), one server at a time. After
# start and a settle period, each is measured idle:
#  - own memory: Pss_Anon + SwapPss (smaps_rollup), and Rss
#  - CPU: utime + stime ticks over the idle window
# usage (WSL, ~/mr setup, after plugin_trace/setup_trace.sh):
#   ~/mr/venv/bin/python tools/plugin_mem.py [moonruster binary] [idle seconds]
import os
import pathlib
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import corpus_cmp as cc  # noqa: E402
import plugin_cmp as pc  # noqa: E402

RUST = sys.argv[1] if len(sys.argv) > 1 else str(pathlib.Path.home() / "mr/target/release/moonruster")
IDLE = float(sys.argv[2]) if len(sys.argv) > 2 else 60.0
SETTLE = 20.0


def memory(pid):
    out = {}
    for line in open(f"/proc/{pid}/smaps_rollup"):
        k, _, v = line.partition(":")
        if k in ("Rss", "Pss_Anon", "SwapPss"):
            out[k] = int(v.split()[0])
    out["own"] = out.get("Pss_Anon", 0) + out.get("SwapPss", 0)
    return out


def ticks(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])  # utime + stime


def measure(cmd, env, port, d):
    pc.port_free(port)
    k = cc.pc.start_klippy(d)
    p = subprocess.Popen(cmd, env=env, cwd=str(cc.TREE), stdout=open(d / "logs/stdout", "w"), stderr=subprocess.STDOUT)
    try:
        pc.wait_up(port)
        pc.wait_klippy_ready(port, 60)
        time.sleep(SETTLE)
        t0 = ticks(p.pid)
        time.sleep(IDLE)
        t1 = ticks(p.pid)
        return memory(p.pid), t1 - t0
    finally:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(30)
        except subprocess.TimeoutExpired:
            p.kill()
        k.kill()
        k.wait()


def main():
    dref = cc.setup_instance("cref", 7060)
    drust = cc.setup_instance("crust", 7070)
    env = dict(os.environ)
    ref = measure([str(pc.VENV_PY), "moonraker/moonraker.py", "-d", str(dref), "-l", str(dref / "logs/moonraker.log")],
                  env, 7060, dref)
    renv = dict(env, MOONRUSTER_MOONRAKER_DIR=str(cc.TREE), MOONRUSTER_PYTHON=str(pc.VENV_PY))
    rust = measure([RUST, "-d", str(drust), "-l", str(drust / "logs/moonraker.log")], renv, 7070, drust)
    print(f"idle window {IDLE:.0f}s after {SETTLE:.0f}s settle, corpus components loaded")
    print(f"{'':10} {'own kB':>8} {'Rss kB':>8} {'CPU ticks':>10}")
    for name, (m, t) in (("moonraker", ref), ("moonruster", rust)):
        print(f"{name:10} {m['own']:>8} {m['Rss']:>8} {t:>10}")
    (mr, tr), (mu, tu) = ref, rust
    print(f"moonruster/moonraker: own {mu['own'] / mr['own']:.2f}, CPU {tu / max(tr, 1):.2f}")


if __name__ == "__main__":
    main()
