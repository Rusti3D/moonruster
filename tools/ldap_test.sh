#!/bin/bash
# LDAP login comparison against tools/mock_ldap.py (needs /tmp/ldapvenv with
# ldaptor).  Adds an [ldap] section to both instances, runs the logins,
# then restores the configs.
cd "$(dirname "$0")"
/tmp/ldapvenv/bin/python mock_ldap.py > /tmp/mock_ldap.log 2>&1 &
MOCK=$!
sleep 2
for i in ref rust; do
    D=~/mr/inst/$i
    # keep an older backup: it is the original if a previous run was killed
    [ -f $D/config/moonraker.conf.bak ] || cp $D/config/moonraker.conf $D/config/moonraker.conf.bak
    cat >> $D/config/moonraker.conf <<'EOF'

[ldap]
ldap_host: 127.0.0.1
ldap_port: 3389
base_dn: ou=people,dc=example,dc=org
group_dn: cn=printers,ou=groups,dc=example,dc=org
check_dn_case: False
EOF
done
bash reset_both.sh > /dev/null
~/mr/venv/bin/python - <<'PY'
import requests, json
from difftest import REF, RUST, norm, diff
CASES = [
    {"username": "alice", "password": "alicepw", "source": "ldap"},
    {"username": "alice", "password": "wrong", "source": "ldap"},
    {"username": "bob", "password": "bobpw", "source": "ldap"},
    {"username": "nobody", "password": "x", "source": "ldap"},
    {"username": "st*r", "password": "starpw", "source": "ldap"},
    {"username": "al*", "password": "alicepw", "source": "ldap"},
    {"username": "alice", "password": "alicepw"},
]
def run(host):
    out = []
    for c in CASES:
        r = requests.post(f"http://{host}/access/login", json=c, timeout=30)
        b = r.json()
        res = b.get("result")
        if isinstance(res, dict):
            res = {k: ("<tok>" if "token" in k else v) for k, v in res.items()}
        out.append({"status": r.status_code, "result": res, "error": b.get("error", {}).get("message")})
    out.append(requests.get(f"http://{host}/access/users/list").json())
    out.append(requests.get(f"http://{host}/access/info").json())
    for u in ("alice", "bob", "st*r", "al*"):
        requests.delete(f"http://{host}/access/user", json={"username": u})
    return out
a = run(REF)
b = run(RUST)
bad = 0
for i, (x, y) in enumerate(zip(a, b)):
    d = diff(norm(x), norm(y), f"$case{i}")
    if d:
        bad += 1
        print("\n".join(d))
print("ref:", json.dumps(a[:6]))
print(f"\n{len(a) - bad}/{len(a)} checks identical")
PY
for i in ref rust; do
    D=~/mr/inst/$i
    mv $D/config/moonraker.conf.bak $D/config/moonraker.conf
done
kill $MOCK
bash reset_both.sh > /dev/null
