#!/bin/bash
# The rustify parity gate (docs/RUSTIFY.md "Gates"): builds, runs every
# differential harness and compares their summary (tools/gate_summary.py)
# with tools/rustify_gate.expected, recorded on main. A difference fails.
#   tools/rustify_gate.sh            run and compare
#   tools/rustify_gate.sh --record   run and record the expected summary
# (WSL, the ~/mr test setup; about an hour)
# the machine's gate lock (kaliru's gates measure resources here): taken by
# a flock process for this script's lifetime, with the lock fd closed (-o)
# in the script, so nothing it starts (test servers) can inherit and keep it
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    export RUSTIFY_GATE_LOCKED=1
    exec flock -o /tmp/rustify_gates.lock bash "$0" "$@"
fi
cd "$(dirname "$0")/.."
OUT=${GATE_OUT:-$HOME/mr/gate}
mkdir -p "$OUT"
rm -f "$OUT"/*.out
SUITES="power machine_cli machine_dbus mqtt_tls config scenario ext difftest ldap ppc"
bash tools/run_all.sh $SUITES > /dev/null 2>&1
if grep -q "BUILD FAILED" /tmp/mr_done.out; then
    cat /tmp/mr_done.out
    exit 2
fi
for s in $SUITES; do cp "/tmp/mr_$s.out" "$OUT/$s.out"; done
timeout 900 ~/mr/venv/bin/python tools/plugin_cmp.py > "$OUT/plugin_cmp.out" 2>&1
timeout 1800 ~/mr/venv/bin/python tools/corpus_cmp.py > "$OUT/corpus_cmp.out" 2>&1
python3 tools/gate_summary.py "$OUT" > "$OUT/summary"
if [ "$1" = "--record" ]; then
    cp "$OUT/summary" tools/rustify_gate.expected
    echo "recorded tools/rustify_gate.expected"
    cat tools/rustify_gate.expected
    exit 0
fi
python3 tools/gate_compare.py tools/rustify_gate.expected "$OUT/summary" || { echo "(outputs in $OUT)"; exit 1; }
