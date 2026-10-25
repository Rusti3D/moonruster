#!/bin/bash
# Builds moonruster in WSL, target dir kept on the linux filesystem
# the machine's gate lock (kaliru's gates measure resources here): taken by
# a flock process for this script's lifetime, with the lock fd closed (-o)
# in the script, so nothing it starts (test servers) can inherit and keep it
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    export RUSTIFY_GATE_LOCKED=1
    exec flock -o /tmp/rustify_gates.lock bash "$0" "$@"
fi
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=~/mr/target
~/.cargo/bin/cargo build ${1:---release} 2>&1 | grep -E "^(error|warning: unused)|^  --> |^error\[|could not compile|Finished|^\s+\|" | head -${2:-200}
