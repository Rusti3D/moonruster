#!/bin/bash
# Synthetic benchmark: runs a server against tools/mock_klippy.py (status
# updates at RATE Hz) with CLIENTS websocket clients subscribed like Mainsail,
# and prints the CPU it used over 30 s and its own memory.
#
# usage: RATE=20 CLIENTS=2 tools/bench.sh <server command...>
#   tools/bench.sh target/release/moonruster
#   tools/bench.sh ~/moonraker-env/bin/python ~/moonraker/moonraker/moonraker.py
# The clients need tornado: set PY to a python that has it (moonraker's venv).
set -e
cd "$(dirname "$0")/.."
PY=${PY:-python3}
D=$(mktemp -d)
SOCK=/tmp/mock-klippy-$$.sock
mkdir -p $D/config $D/gcodes
cat > $D/config/moonraker.conf <<EOC
[server]
host: 127.0.0.1
port: 7300
klippy_uds_address: $SOCK
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
python3 tools/mock_klippy.py $SOCK ${RATE:-20} & MK=$!
sleep 0.5
"$@" -d $D -n > $D/server.log 2>&1 & P=$!
trap 'kill $P $MK 2>/dev/null; rm -rf $D $SOCK' EXIT
sleep 8
c0=$(awk '{print $14+$15}' /proc/$P/stat)
$PY tools/ws_clients.py 7300 ${CLIENTS:-2} 30
c1=$(awk '{print $14+$15}' /proc/$P/stat)
echo "cpu: $((c1-c0)) ticks (1/100 s) in 30 s, own memory: $(awk '/^Pss_Anon/{print $2" kB"}' /proc/$P/smaps_rollup)"
