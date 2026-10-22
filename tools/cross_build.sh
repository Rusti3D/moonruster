#!/bin/bash
# Builds static (musl) release binaries for printer hosts into dist/.
# Needs rustup targets and cargo-zigbuild + ziglang (pip install ziglang cargo-zigbuild).
# usage: cross_build.sh [target...]
set -e
cd "$(dirname "$0")/.."
TARGETS=${@:-aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf x86_64-unknown-linux-musl}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
mkdir -p dist
for t in $TARGETS; do
    case $t in
        aarch64-*) name=aarch64;;
        armv7-*) name=armv7;;
        arm-*) name=armv6;;
        x86_64-*) name=x86_64;;
        *) name=$t;;
    esac
    echo "### $t"
    cargo zigbuild --release --target "$t"
    cp "$CARGO_TARGET_DIR/$t/release/moonruster" "dist/moonruster-linux-$name"
done
ls -la dist
