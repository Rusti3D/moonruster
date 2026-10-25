#!/bin/bash
# Runs a command under the machine's gate lock (builds, tests, profiling:
# anything that would skew a gate's resource measurement)
#   tools/locked.sh cargo test --release
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    exec 9> /tmp/rustify_gates.lock
    flock 9
    export RUSTIFY_GATE_LOCKED=1
fi
exec "$@"
