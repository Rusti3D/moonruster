#!/bin/bash
# Starts the reference python moonraker for instance "ref" (background)
D=~/mr/inst/ref
pkill -f "moonraker.py -d $D" 2>/dev/null
sleep 0.5
cd ~/mr/moonraker
nohup ~/mr/venv/bin/python moonraker/moonraker.py -d "$D" > "$D/logs/moonraker.stdout" 2>&1 &
for i in $(seq 1 100); do
    curl -s localhost:7010/server/info > /dev/null && break
    sleep 0.2
done
curl -s localhost:7010/server/info | head -c 600; echo
