#!/bin/sh
# Resource measurements on the CC1 (POSIX sh: BusyBox), docs/PLUGINS.md
# "Resource gates". Run from /data/mrtest holding moonruster-gnu,
# moonruster-musl, mock_klippy.py, ws_clients.py, timelapse.py.
# Side port 7301 and its own data folders: the printer's own Moonruster and
# klippy keep running untouched; nothing moves the printer.
#  A: no Python components, musl vs glibc build (idle; 20 status updates/s
#     to 2 websocket clients)
#  B: one Python component (timelapse), Python Moonraker (the CC1's own tree)
#     vs Moonruster glibc, same tree and config
# Own memory = Pss_Anon + SwapPss (zram). CPU = utime+stime ticks in a window.
# usage: sh cc1_plugins_measure.sh [settle_s] [window_s]
SETTLE=${1:-60}
WIN=${2:-60}
H=$(pwd)
PY=/usr/bin/python3
own() { a=$(awk '/^Pss_Anon/{print $2}' /proc/$1/smaps_rollup); s=$(awk '/^SwapPss/{print $2}' /proc/$1/smaps_rollup); echo $((a + s)); }
ticks() { awk '{print $14+$15}' /proc/$1/stat; }

# the Moonraker tree both servers use: the CC1's own, plus timelapse
if [ ! -d $H/mtree ]; then
    cp -r /usr/share/moonraker $H/mtree
    cp $H/timelapse.py $H/mtree/moonraker/components/timelapse.py
fi

conf() {  # data folder, klippy socket, with_component
    mkdir -p $1/config $1/gcodes $1/logs $1/tl $1/tlf
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
    if [ "$3" = yes ]; then
        cat >> $1/config/moonraker.conf <<EOC

[timelapse]
output_path: $1/tl
frame_path: $1/tlf
EOC
    fi
}

# run LABEL MODE(idle|load) WITH_COMPONENT CMD...
run() {
    label=$1; mode=$2; comp=$3; shift 3
    D=$H/run.$$
    rm -rf $D
    S=$H/mk.sock
    rm -f $S
    MK=""
    if [ "$mode" = load ]; then
        $PY $H/mock_klippy.py $S 20 > /dev/null 2>&1 &
        MK=$!
        sleep 1
    fi
    conf $D $S $comp
    "$@" -d $D -l $D/logs/moonraker.log > $D/out 2>&1 &
    P=$!
    sleep $SETTLE
    if [ "$mode" = load ]; then
        $PY $H/ws_clients.py 7301 2 $((WIN + 5)) > $D/ws.out 2>&1 &
        WS=$!
        sleep 3
    fi
    t0=$(ticks $P)
    sleep $WIN
    t1=$(ticks $P)
    echo "$label $mode: own $(own $P) kB, cpu $((t1 - t0)) ticks/${WIN}s, rss $(awk '/^VmRSS/{print $2}' /proc/$P/status) kB"
    [ -n "$WS" ] && { wait $WS 2>/dev/null; cat $D/ws.out; WS=""; }
    kill $P; wait $P 2>/dev/null
    [ -n "$MK" ] && kill $MK
    grep -i -E "unable to load|traceback" $D/logs/moonraker.log | head -n 3
    rm -rf $D $S
}

echo "### A: no components (settle ${SETTLE}s, window ${WIN}s)"
for i in 1 2; do
    run musl idle no $H/moonruster-musl
    run glibc idle no $H/moonruster-gnu
done
for i in 1 2; do
    run musl load no $H/moonruster-musl
    run glibc load no $H/moonruster-gnu
done

echo "### B: timelapse component"
export MOONRUSTER_MOONRAKER_DIR=$H/mtree MOONRUSTER_PYTHON=$PY
for i in 1 2; do
    run moonraker idle yes $PY $H/mtree/moonraker/moonraker.py
    run moonruster idle yes $H/moonruster-gnu
done
for i in 1 2; do
    run moonraker load yes $PY $H/mtree/moonraker/moonraker.py
    run moonruster load yes $H/moonruster-gnu
done
echo "### done"
