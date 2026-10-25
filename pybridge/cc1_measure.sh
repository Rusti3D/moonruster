#!/bin/sh
# Phase 1 measurements on a printer (POSIX sh: COSMOS has busybox, no bash).
# Run from a directory holding: proto, moonruster-musl, moonruster-gnu,
# mock_klippy.py, ws_clients.py. Uses port 7301 and its own data folders,
# so the printer's own Moonruster keeps running untouched.
# usage: sh cc1_measure.sh PYTHON
PY=${1:-/usr/bin/python3}
H=$(pwd)
own() {  # own memory of pid $1: Pss_Anon + SwapPss (zram swap would hide pages from Pss_Anon)
    a=$(awk '/^Pss_Anon/{print $2}' /proc/$1/smaps_rollup)
    s=$(awk '/^SwapPss/{print $2}' /proc/$1/smaps_rollup)
    echo "own $((a + s)) kB (Pss_Anon $a + SwapPss $s), Threads $(awk '/^Threads/{print $2}' /proc/$1/status)"
}
cpu() { awk '{print $14+$15}' /proc/$1/stat; }
conf() {  # data folder $1, klippy socket $2
    mkdir -p $1/config $1/gcodes
    cat > $1/config/moonraker.conf <<EOC
[server]
host: 127.0.0.1
port: 7301
klippy_uds_address: $2
[authorization]
trusted_clients:
    127.0.0.0/8
[machine]
provider: none
validate_service: False
[history]
[update_manager]
enable_auto_refresh: False
EOC
}

if [ "$2" != "noproto" ]; then
    echo "### prototype ($PY)"
    ./proto $PY
fi

TUNED=glibc.malloc.top_pad=0:glibc.malloc.trim_threshold=0
for v in moonruster-musl moonruster-gnu moonruster-gnu+tuned; do
    b=${v%+tuned}
    if [ "$v" != "$b" ]; then export GLIBC_TUNABLES=$TUNED; else unset GLIBC_TUNABLES; fi
    for i in 1 2; do
        D=$H/run.$b.$i
        conf $D $H/none.sock
        ./$b -d $D -n > $D/log 2>&1 &
        P=$!
        sleep 20
        echo "IDLE $v run $i: $(own $P)"
        kill $P; wait $P 2>/dev/null; rm -rf $D
    done
    for i in 1 2; do
        D=$H/run.$b.load$i
        S=$H/mk.sock
        conf $D $S
        $PY mock_klippy.py $S 20 > /dev/null 2>&1 &
        MK=$!
        sleep 1
        ./$b -d $D -n > $D/log 2>&1 &
        P=$!
        sleep 10
        c0=$(cpu $P)
        $PY ws_clients.py 7301 2 30 > /dev/null 2>&1
        c1=$(cpu $P)
        echo "LOAD $v run $i: cpu $((c1 - c0)) ticks/30 s, $(own $P)"
        kill $P $MK; wait $P $MK 2>/dev/null; rm -rf $D $S
    done
done
