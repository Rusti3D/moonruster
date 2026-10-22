#!/usr/bin/env python3
# Power device comparison used by power_test.sh.  Each server runs the same
# steps against a freshly reset mock; responses, notifications and the
# requests the mock received are compared.
import time
import json
import requests
from difftest import REF, RUST, norm, diff, http
from scenario import Listener

MOCK = "http://127.0.0.2"
DEVS = ["tp", "tp_child", "tp_timer", "tas2", "shl2", "st", "hs", "ha", "lox", "hue", "huegrp"]

def mock_log():
    log = requests.get(f"{MOCK}/_log").json()
    # polling may repeat identical status requests, collapse repeats
    out = []
    for e in log:
        if not out or out[-1] != e:
            out.append(e)
    return out

def steps():
    s = [("GET", "/machine/device_power/devices", {})]
    for d in DEVS:
        s.append(("GET", f"/machine/device_power/device?device={d}", {}))
        s.append(("POST", f"/machine/device_power/device?device={d}&action=on", {}))
        s.append(("POST", f"/machine/device_power/device?device={d}&action=off", {}))
        s.append(("POST", f"/machine/device_power/device?device={d}&action=toggle", {}))
    s.append(("GET", "/machine/device_power/status?" + "&".join(DEVS), {}))
    s.append(("POST", "/machine/device_power/off?" + "&".join(DEVS), {}))
    return s

def run(host):
    lis = Listener(host)
    time.sleep(3.0)
    lis.take()
    requests.post(f"{MOCK}/_log")
    out = []
    for m, p, kw in steps():
        r = http(host, m, p, **kw)
        time.sleep(0.6)
        out.append((p, r, lis.take(), mock_log()))
    return out

def main():
    # wait for both servers' devices to finish their init requests
    time.sleep(3)
    ra = run(REF)
    rb = run(RUST)
    bad = 0
    for (p, a, ea, ma), (_, b, eb, mb) in zip(ra, rb):
        d = diff(norm(a), norm(b), "$resp") + diff(norm(ea), norm(eb), "$notify") + diff(ma, mb, "$mock")
        if d:
            bad += 1
            print(f"=== {p}")
            for line in d[:30]:
                print("   ", line)
    # show what the reference saw so an all-error run is visible
    nreq = sum(len(m) for (_, _, _, m) in ra)
    print(f"mock requests (ref): {nreq}")
    print("final status (ref):", json.dumps(ra[-2][1].get("body"))[:600])
    print(f"\n{len(ra) - bad}/{len(ra)} steps identical")

if __name__ == "__main__":
    main()
