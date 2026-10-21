#!/bin/bash
# Starts a batch klippy process for a test instance (background)
# usage: start_klippy.sh <name>
NAME=$1
D=~/mr/inst/$NAME
pkill -f "klippy.py $D/config/printer.cfg" 2>/dev/null
rm -f "$D/comms/klippy.sock" "$D/comms/klippy_pty"
cd ~/mr/klipper/klippy
nohup ~/mr/venv/bin/python klippy.py "$D/config/printer.cfg" -o /dev/null \
    -d ~/mr/klipper.dict \
    -a "$D/comms/klippy.sock" -I "$D/comms/klippy_pty" \
    -l "$D/logs/klippy.log" > "$D/logs/klippy.stdout" 2>&1 &
for i in $(seq 1 100); do
    [ -S "$D/comms/klippy.sock" ] && break
    sleep 0.1
done
ls -la "$D/comms"
