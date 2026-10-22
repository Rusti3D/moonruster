#!/usr/bin/env python3
# Machine component comparison used by machine_test.sh
import time
import json
from difftest import REF, RUST, norm, diff, http
from scenario import Listener

STEPS = [
    ("GET", "/machine/system_info", {}),
    ("POST", "/machine/services/stop?service=mrtest", {}),
    ("GET", "/machine/system_info", {}),
    ("POST", "/machine/services/start?service=mrtest", {}),
    ("POST", "/machine/services/restart?service=mrtest", {}),
    ("POST", "/machine/services/stop?service=nope", {}),
    ("GET", "/machine/sudo/info?check_access=true", {}),
    ("GET", "/server/info", {}),
]

def run(host):
    lis = Listener(host)
    time.sleep(0.5)
    lis.take()
    out = []
    for m, p, kw in STEPS:
        r = http(host, m, p, **kw)
        time.sleep(2.0)
        evs = [e for e in lis.take() if e[0] != "notify_filelist_changed"]
        out.append((p, r, evs))
    return out

def main():
    ra = run(REF)
    rb = run(RUST)
    bad = 0
    for (p, a, ea), (_, b, eb) in zip(ra, rb):
        d = diff(norm(a), norm(b), "$resp") + diff(norm(ea), norm(eb), "$notify")
        if d:
            bad += 1
            print(f"=== {p}")
            for line in d[:40]:
                print("   ", line)
    print(f"\n{len(STEPS) - bad}/{len(STEPS)} steps identical")

if __name__ == "__main__":
    main()
