#!/bin/bash
# The rustify resource gate (docs/RUSTIFY.md "Gates"): resource use must only
# get better. Measures the release binary (WSL, the ~/mr setup):
#  - idle, and load (tools/mock_klippy.py at 20 status updates/s, 2 websocket
#    clients subscribed like Mainsail: tools/ws_clients.py), each without and
#    with a Python component (timelapse from ~/mr/trace's Moonraker tree)
#  - per run: CPU (ns, /proc/<pid>/task/*/schedstat), peak RSS (VmHWM),
#    anonymous PSS (smaps_rollup); RUNS runs, the minimum kept
#  - the x86-64 binary size (and armv7 with --armv7: tools/scratch_cc1build.sh)
#   tools/resource_gate.sh            measure and compare with the baseline
#   tools/resource_gate.sh --record   measure and record the baseline
#   REF=<binary> tools/resource_gate.sh   measure REF and the binary in
#                                     alternating runs and compare with REF:
#                                     drift of the machine between sessions cancels
# A metric fails when it exceeds the baseline's worst run by more than
# TOL percent (default 3).
# the machine's gate lock (kaliru's gates measure resources here): taken by
# a flock process for this script's lifetime, with the lock fd closed (-o)
# in the script, so nothing it starts (test servers) can inherit and keep it
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    export RUSTIFY_GATE_LOCKED=1
    exec flock -o /tmp/rustify_gates.lock bash "$0" "$@"
fi
cd "$(dirname "$0")/.."
RUNS=${RUNS:-5}
SETTLE=${SETTLE:-20}
WIN=${WIN:-30}
TOL=${TOL:-3}
BIN=${BIN:-$HOME/mr/target/release/moonruster}
PY=$HOME/mr/venv/bin/python
TREE=$HOME/mr/trace/moonraker
W=$HOME/mr/rgate
BASE=tools/resource_gate.baseline
mkdir -p "$W"

cpu_ns() { cat /proc/$1/task/*/schedstat 2>/dev/null | awk '{s += $1} END {print s}'; }
anon() { awk '/^Pss_Anon/{print $2}' /proc/$1/smaps_rollup; }
hwm() { awk '/^VmHWM/{print $2}' /proc/$1/status; }

conf() {  # dir socket with_component
    mkdir -p $1/config $1/gcodes $1/logs $1/tl $1/tlf
    cat > $1/config/moonraker.conf <<EOC
[server]
host: 127.0.0.1
port: 7311
klippy_uds_address: $2

[authorization]
trusted_clients:
    127.0.0.0/8

[machine]
provider: none
validate_service: False

[history]
EOC
    if [ "$3" = yes ]; then
        printf '\n[timelapse]\noutput_path: %s/tl\nframe_path: %s/tlf\n' "$1" "$1" >> $1/config/moonraker.conf
    fi
}

# one run: prints "cpu_ns hwm_kb anon_kb"
run() {  # mode with_component [binary]
    D=$W/run
    rm -rf "$D"
    S=$W/mk.sock
    rm -f "$S"
    $PY tools/mock_klippy.py "$S" $([ "$1" = load ] && echo 20 || echo 0.2) > /dev/null 2>&1 &
    MK=$!
    sleep 1
    conf "$D" "$S" "$2"
    MOONRUSTER_MOONRAKER_DIR=$TREE MOONRUSTER_PYTHON=$PY "${3:-$BIN}" -d "$D" -l "$D/logs/moonraker.log" > "$D/out" 2>&1 &
    P=$!
    sleep "$SETTLE"
    WS=""
    if [ "$1" = load ]; then
        $PY tools/ws_clients.py 7311 2 $((WIN + 5)) > /dev/null 2>&1 &
        WS=$!
        sleep 3
    fi
    c0=$(cpu_ns $P)
    sleep "$WIN"
    c1=$(cpu_ns $P)
    echo "$((c1 - c0)) $(hwm $P) $(anon $P)"
    [ -n "$WS" ] && wait $WS 2>/dev/null
    kill $P $MK 2>/dev/null
    wait $P 2>/dev/null
}

# min and max of each column over the runs
minmax() {  # key
    awk -v k="$1" '
        { for (i = 1; i <= 3; i++) { if (NR == 1 || $i < mn[i]) mn[i] = $i; if (NR == 1 || $i > mx[i]) mx[i] = $i } }
        END { split("cpu_ns hwm_kb anon_kb", n, " ");
              for (i = 1; i <= 3; i++) printf "%s.%s %d %d\n", k, n[i], mn[i], mx[i] }'
}

measure() {
    : > "$W/ref"
    for mode in idle load; do
        for comp in no yes; do
            : > "$W/vals"
            : > "$W/refvals"
            for i in $(seq "$RUNS"); do
                [ -n "$REF" ] && run $mode $comp "$REF" >> "$W/refvals"
                run $mode $comp >> "$W/vals"
            done
            minmax "${mode}_${comp}comp" < "$W/vals"
            [ -n "$REF" ] && minmax "${mode}_${comp}comp" < "$W/refvals" >> "$W/ref"
        done
    done
    s=$(stat -c %s "$BIN")
    echo "binary.x86_64 $s $s"
    if [ -n "$REF" ]; then
        r=$(stat -c %s "$REF")
        echo "binary.x86_64 $r $r" >> "$W/ref"
    fi
    if [ "$ARMV7" = 1 ]; then
        bash tools/scratch_cc1build.sh > /dev/null 2>&1
        a=$(stat -c %s "$HOME/mr/cc1run/moonruster-gnu")
        echo "binary.armv7 $a $a"
    fi
}

[ "$1" = "--armv7" ] || [ "$2" = "--armv7" ] && ARMV7=1
OUT=$W/current
measure > "$OUT"
[ -n "$REF" ] && BASE=$W/ref
if [ "$1" = "--record" ]; then
    cp "$OUT" "$BASE"
    echo "recorded $BASE"
    cat "$BASE"
    exit 0
fi
# a metric fails when its best run is worse than the baseline's worst run by
# more than TOL percent
awk -v tol="$TOL" 'NR == FNR { bmin[$1] = $2; bmax[$1] = $3; next }
    ($1 in bmax) {
        lim = bmax[$1] * (1 + tol / 100)
        st = ($2 > lim) ? "FAIL" : "ok"
        if (st == "FAIL") bad = 1
        printf "%-28s %12d  (baseline %d..%d)  %s\n", $1, $2, bmin[$1], bmax[$1], st
    }
    END { print (bad ? "RESOURCE GATE FAIL" : "RESOURCE GATE PASS"); exit bad }' "$BASE" "$OUT"
