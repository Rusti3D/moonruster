#!/bin/bash
# Builds an atmega2560 klipper.dict matching the checked out klipper version
set -e
export DEBIAN_FRONTEND=noninteractive
if ! command -v avr-gcc > /dev/null; then
    apt-get update -qq && apt-get install -y -qq gcc-avr avr-libc binutils-avr > /dev/null
fi
cd ~/mr/klipper
cat > .config <<CFG
CONFIG_MACH_AVR=y
CONFIG_MACH_atmega2560=y
CONFIG_CLOCK_FREQ=16000000
CFG
make olddefconfig > /dev/null
make -j8 > /dev/null 2>&1 || make
cp out/klipper.dict ~/mr/klipper.dict
echo "dict built: $(grep -o '"version": "[^"]*"' ~/mr/klipper.dict)"
