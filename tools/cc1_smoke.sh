#!/bin/sh
# smoke test on the CC1: both servers load the timelapse component (side port)
H=/data/mrtest
cd $H
[ -d $H/mtree ] || { cp -r /usr/share/moonraker $H/mtree; cp $H/timelapse.py $H/mtree/moonraker/components/timelapse.py; }
for who in moonraker moonruster; do
    D=$H/smoke.$who
    rm -rf $D; mkdir -p $D/config $D/logs $D/tl $D/tlf
    printf '[server]\nhost: 127.0.0.1\nport: 7301\nklippy_uds_address: %s/none.sock\n[authorization]\ntrusted_clients:\n    127.0.0.0/8\n[machine]\nprovider: none\nvalidate_service: False\n[timelapse]\noutput_path: %s/tl\nframe_path: %s/tlf\n' $D $D $D > $D/config/moonraker.conf
    if [ $who = moonraker ]; then
        /usr/bin/python3 $H/mtree/moonraker/moonraker.py -d $D -l $D/logs/moonraker.log > $D/out 2>&1 &
    else
        MOONRUSTER_MOONRAKER_DIR=$H/mtree MOONRUSTER_PYTHON=/usr/bin/python3 $H/moonruster-gnu -d $D -l $D/logs/moonraker.log > $D/out 2>&1 &
    fi
    P=$!
    sleep 25
    echo "== $who: $(wget -qO- http://127.0.0.1:7301/server/info | head -c 300)"
    grep -E "timelapse|Unable|Error|Traceback" $D/logs/moonraker.log | head -n 5
    kill $P; wait $P 2>/dev/null
    rm -rf $D
done
