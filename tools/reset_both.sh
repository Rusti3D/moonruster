#!/bin/bash
# Restarts both klippy instances and both servers, waits for klippy ready
cd "$(dirname "$0")"
bash start_klippy.sh ref > /dev/null
bash start_klippy.sh rust > /dev/null
bash start_ref.sh > /dev/null
bash start_rust.sh "$@" > /dev/null
for port in 7010 7020; do
    for i in $(seq 1 100); do
        curl -s localhost:$port/server/info | grep -q '"klippy_state":"ready"' && break
        sleep 0.2
    done
done
sleep 1.5
echo "ref: $(curl -s localhost:7010/server/info | grep -o '"klippy_state":"[a-z]*"')"
echo "rust: $(curl -s localhost:7020/server/info | grep -o '"klippy_state":"[a-z]*"')"
