#!/usr/bin/env python3
# Config parsing differential test.  For each variant, extra config text and
# include files are written into both test instances, both servers are
# restarted, and /server/config, /server/info and /server/webcams/list are
# compared.  The original configs are restored afterwards.
#
# usage: config_cmp.py [variant-filter]
import os
import sys
import json
import time
import shutil
import subprocess
import requests
from difftest import REF, RUST, REF_DIR, RUST_DIR, norm, diff

HERE = os.path.dirname(os.path.abspath(__file__))
INST = {"ref": (REF, REF_DIR), "rust": (RUST, RUST_DIR)}

VARIANTS = {
    "include_comments": {
        "main": """
# full line comment
; semicolon comment
[include inc/*.cfg]

[webcam cam_a]
stream_url: http://host/stream#frag   # inline comment
snapshot_url: /snap \\#notcomment
rotation: 90 ; trailing
location: printer
""",
        "files": {
            "inc/a.cfg": "[webcam cam_b]\n\tstream_url: /b\nservice:mjpegstreamer\n[include deep/*.cfg]\n",
            "inc/b.cfg": "[webcam cam_d]\nstream_url = /d\nicon :   mdiWebcam  \n",
            "inc/deep/x.cfg": "[webcam cam_c]\nstream_url:\n    /c\n    second line\nextra_data:\n",
        },
    },
    "warnings": {
        "main": """
[server]
bogus_server_opt: 1

[webcam bad_rot]
stream_url: /x
rotation: 45

[unknown_component_xyz]
foo: bar

[authorization]
enable_api_key: maybe
""",
        "replace": {"[authorization]\ntrusted_clients:": "[authorization_old]\ntrusted_clients:"},
    },
    "case_and_delims": {
        "main": """
[Webcam Big]
Stream_URL: /big
TARGET_FPS = 30
empty_value:
spaced key : spaced value
""",
    },
    "default_section": {
        "main": """
[DEFAULT]
default_opt: inherited

[webcam dflt]
stream_url: /dflt
""",
    },
    "duplicate_option": {
        "main": """
[webcam dup]
stream_url: /one
stream_url: /two
""",
    },
    "syntax_error": {
        "main": """
webcam broken]
stream_url: /x
""",
    },
    "missing_include": {
        "main": "\n[include nope/*.cfg]\n",
    },
    "include_mid_section": {
        "main": """
[webcam mid]
stream_url: /mid
[include inc/m.cfg]
location: after_include
""",
        "files": {"inc/m.cfg": "[webcam m2]\nstream_url: /m2\n"},
    },
    "secrets_template": {
        "main": """
[webcam sec]
stream_url: http://{secrets.cam.host}/stream
""",
        "secrets": "[cam]\nhost: camhost.local\n",
    },
}

def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)

def get(host, path):
    try:
        r = requests.get(f"http://{host}{path}", timeout=10)
        return r.json()
    except Exception as e:
        return {"error": str(e)}

def wait_up(host):
    for _ in range(100):
        try:
            requests.get(f"http://{host}/server/info", timeout=2)
            return True
        except Exception:
            time.sleep(0.2)
    return False

def apply(name, var):
    for inst, (host, d) in INST.items():
        cfg = os.path.join(d, "config", "moonraker.conf")
        with open(cfg + ".orig") as f:
            data = f.read()
        for a, b in var.get("replace", {}).items():
            data = data.replace(a, b)
        with open(cfg, "w") as f:
            f.write(data + var.get("main", ""))
        for rel, content in var.get("files", {}).items():
            p = os.path.join(d, "config", rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write(content)
        sec = os.path.join(d, "moonraker.secrets")
        if "secrets" in var:
            with open(sec, "w") as f:
                f.write(var["secrets"])
        elif os.path.exists(sec):
            os.remove(sec)

def cleanup_files():
    for inst, (host, d) in INST.items():
        shutil.rmtree(os.path.join(d, "config", "inc"), ignore_errors=True)
        sec = os.path.join(d, "moonraker.secrets")
        if os.path.exists(sec):
            os.remove(sec)

def unify(v):
    # instance specific values and python set ordering of sections
    if isinstance(v, dict):
        return {k: unify(x) for k, x in sorted(v.items()) if k != "uid"}
    if isinstance(v, list):
        return [unify(x) for x in v]
    if isinstance(v, str):
        return v.replace("rust", "ref").replace("7020", "7010")
    return v

def snapshot(host):
    info = get(host, "/server/info")
    res = info.get("result", info)
    cfg = get(host, "/server/config")
    cams = get(host, "/server/webcams/list")
    if isinstance(res, dict):
        res = {k: res.get(k) for k in ("warnings", "failed_components")}
    c = cfg.get("result", cfg)
    if isinstance(c, dict):
        c = {k: c.get(k) for k in ("config", "orig", "files")}
    return {"info": res, "config": c, "webcams": cams.get("result", cams)}

def main():
    filt = sys.argv[1] if len(sys.argv) > 1 else ""
    for inst, (host, d) in INST.items():
        cfg = os.path.join(d, "config", "moonraker.conf")
        shutil.copy(cfg, cfg + ".orig")
    bad = 0
    total = 0
    try:
        for name, var in VARIANTS.items():
            if filt and filt not in name:
                continue
            total += 1
            cleanup_files()
            apply(name, var)
            sh(f"bash {HERE}/start_ref.sh")
            sh(f"bash {HERE}/start_rust.sh")
            wait_up(REF)
            wait_up(RUST)
            time.sleep(1.0)
            a = snapshot(REF)
            b = snapshot(RUST)
            d = diff(unify(norm(a)), unify(norm(b)), f"${name}")
            if d:
                bad += 1
                print(f"=== {name}")
                for line in d[:30]:
                    print("   ", line)
    finally:
        cleanup_files()
        for inst, (host, d) in INST.items():
            cfg = os.path.join(d, "config", "moonraker.conf")
            shutil.move(cfg + ".orig", cfg)
        sh(f"bash {HERE}/start_ref.sh")
        sh(f"bash {HERE}/start_rust.sh")
    print(f"\n{total - bad}/{total} variants identical")

if __name__ == "__main__":
    main()
