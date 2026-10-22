#!/bin/bash
# Object processing (preprocess_cancellation) comparison.  Enables
# enable_object_processing on both instances, uploads the ppc_* samples and
# compares the processed files byte for byte and their metadata.
cd "$(dirname "$0")"
~/mr/venv/bin/python gen_samples.py samples
for i in ref rust; do
    D=~/mr/inst/$i
    # keep an older backup: it is the original if a previous run was killed
    [ -f $D/config/moonraker.conf.bak ] || cp $D/config/moonraker.conf $D/config/moonraker.conf.bak
    printf "\n[file_manager]\nenable_object_processing: True\n" >> $D/config/moonraker.conf
done
bash reset_both.sh > /dev/null
~/mr/venv/bin/python - <<'PY'
import os, time, hashlib, requests, difflib
from difftest import REF, RUST, REF_DIR, RUST_DIR, norm, diff
NAMES = sorted(n for n in os.listdir("samples") if n.startswith("ppc_"))
def run(host, d):
    requests.delete(f"http://{host}/server/files/directory?path=gcodes/ppc&force=true")
    out = {}
    for n in NAMES:
        with open(os.path.join("samples", n), "rb") as f:
            requests.post(f"http://{host}/server/files/upload", files={"file": (n, f.read())}, data={"path": "ppc"})
    time.sleep(3)
    for n in NAMES:
        with open(os.path.join(d, "gcodes", "ppc", n), "rb") as f:
            data = f.read()
        md = requests.get(f"http://{host}/server/files/metadata?filename=ppc/{n}").json().get("result", {})
        md = {k: v for k, v in md.items() if k not in ("modified", "uuid", "print_start_time", "job_id")}
        out[n] = (data, md)
    requests.delete(f"http://{host}/server/files/directory?path=gcodes/ppc&force=true")
    return out
a = run(REF, REF_DIR)
b = run(RUST, RUST_DIR)
bad = 0
for n in NAMES:
    (da, ma), (db, mb) = a[n], b[n]
    processed = b"EXCLUDE_OBJECT_DEFINE" in da
    msg = []
    if da != db:
        msg.append("file differs:")
        msg += list(difflib.unified_diff(da.decode(errors="replace").splitlines(), db.decode(errors="replace").splitlines(), "ref", "rust", lineterm="", n=1))[:30]
    msg += diff(norm(ma), norm(mb), "$metadata")
    status = "processed" if processed else "unchanged"
    if msg:
        bad += 1
        print(f"=== {n} ({status} by ref)")
        print("\n".join(msg))
    else:
        print(f"ok  {n} ({status}, {hashlib.sha1(da).hexdigest()[:10]})")
print(f"\n{len(NAMES) - bad}/{len(NAMES)} files identical")
PY
for i in ref rust; do
    D=~/mr/inst/$i
    mv $D/config/moonraker.conf.bak $D/config/moonraker.conf
done
bash reset_both.sh > /dev/null
