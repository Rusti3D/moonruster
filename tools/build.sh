#!/bin/bash
# Builds moonruster in WSL, target dir kept on the linux filesystem
cd "$(dirname "$0")/.."
# builds and test runs take the machine's gate lock (kaliru's gates
# measure resources on it), unless a gate run already holds it
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    exec 9> /tmp/rustify_gates.lock
    flock 9
    export RUSTIFY_GATE_LOCKED=1
fi
export CARGO_TARGET_DIR=~/mr/target
~/.cargo/bin/cargo build ${1:---release} 2>&1 | grep -E "^(error|warning: unused)|^  --> |^error\[|could not compile|Finished|^\s+\|" | head -${2:-200}
