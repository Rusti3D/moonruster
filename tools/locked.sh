#!/bin/bash
# Runs a command under the machine's gate lock (builds, tests, profiling:
# anything that would skew a gate's resource measurement). The lock is held
# by the flock process, its fd closed (-o) in the command, so nothing the
# command starts can inherit and keep it.
#   tools/locked.sh cargo test --release
if [ -n "$RUSTIFY_GATE_LOCKED" ]; then
    exec "$@"
fi
export RUSTIFY_GATE_LOCKED=1
exec flock -o /tmp/rustify_gates.lock "$@"
