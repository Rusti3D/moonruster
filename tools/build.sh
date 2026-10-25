#!/bin/bash
# Builds moonruster in WSL, target dir kept on the linux filesystem
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=~/mr/target
~/.cargo/bin/cargo build ${1:---release} 2>&1 | grep -E "^(error|warning: unused)|^  --> |^error\[|could not compile|Finished|^\s+\|" | head -${2:-200}
