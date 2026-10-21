#!/bin/bash
# Starts moonruster for instance "rust" (background)
D=~/mr/inst/rust
pkill -f "moonruster -d $D" 2>/dev/null
sleep 0.5
nohup ~/mr/target/release/moonruster -d "$D" "$@" > "$D/logs/moonruster.stdout" 2>&1 &
for i in $(seq 1 50); do
    curl -s localhost:7020/server/info > /dev/null && break
    sleep 0.2
done
curl -s localhost:7020/server/info | head -c 1500; echo
