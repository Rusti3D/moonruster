#!/bin/bash
# Starts the trace instance (setup_trace.sh): its klippy, the spoolman and
# device mocks if they are not running, and Python Moonraker under tracer.py.
# Trace records: ~/mr/inst/trace/logs/trace.jsonl (appended across runs).
# usage: run_trace.sh
HERE="$(cd "$(dirname "$0")" && pwd)"
D=~/mr/inst/trace
COMPONENTS=timelapse,uboe_metadata,additional_pre_print_checks,spool_change_estimator,geeks3d,helix_print,filaman,mmu_server
bash "$HERE/../start_klippy.sh" trace > /dev/null
curl -s -o /dev/null localhost:7912 || (cd "$HERE/.." && nohup ~/mr/venv/bin/python mock_spoolman.py > /tmp/mock_spoolman.log 2>&1 &)
curl -s -o /dev/null localhost:7913 || (cd "$HERE/.." && nohup ~/mr/venv/bin/python mock_devices.py > /tmp/mock_devices.log 2>&1 &)
pkill -f "tracer.py $HOME/mr/trace/moonraker" 2>/dev/null
for i in $(seq 1 100); do pgrep -f "tracer.py $HOME/mr/trace/moonraker" > /dev/null || break; sleep 0.1; done
TRACE_OUT="$D/logs/trace.jsonl" TRACE_COMPONENTS="$COMPONENTS" \
    nohup ~/mr/venv/bin/python "$HERE/tracer.py" ~/mr/trace/moonraker -d "$D" \
    > "$D/logs/moonraker.stdout" 2>&1 &
for i in $(seq 1 150); do
    curl -s localhost:7030/server/info | grep -q '"klippy_state":"ready"' && break
    sleep 0.2
done
curl -s localhost:7030/server/info; echo
