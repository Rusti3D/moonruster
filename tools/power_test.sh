#!/bin/bash
# Compares power devices (tplink, tasmota, shelly, smartthings, homeseer,
# homeassistant, loxone, hue) against tools/mock_power.py.  Adds the device
# sections to both test instances, runs power_cmp.py, then restores them.
cd "$(dirname "$0")"
pkill -f mock_power.py 2>/dev/null
~/mr/venv/bin/python mock_power.py > /tmp/mock_power.log 2>&1 &
MOCK=$!
sleep 1
for i in ref rust; do
    D=~/mr/inst/$i
    cp $D/config/moonraker.conf $D/config/moonraker.conf.bak
    cat >> $D/config/moonraker.conf <<'EOF'

[power tp]
type: tplink_smartplug
address: 127.0.0.2

[power tp_child]
type: tplink_smartplug
address: 127.0.0.2
output_id: 1

[power tp_timer]
type: tplink_smartplug
address: 127.0.0.2
timer: 5

[power tas2]
type: tasmota
address: 127.0.0.2
password: pw
timer: 3

[power shl2]
type: shelly
address: 127.0.0.2
user: admin
password: secret
timer: 2

[power st]
type: smartthings
address: 127.0.0.2
protocol: http
device: abc-123
token: STTOKEN

[power hs]
type: homeseer
address: 127.0.0.2
port: 80
device: 42
user: hsuser
password: hspw

[power ha]
type: homeassistant
address: 127.0.0.2
port: 80
device: switch.plug
token: HATOKEN
status_delay: 0.2

[power lox]
type: loxonev1
address: 127.0.0.2
output_id: out1

[power hue]
type: hue
address: 127.0.0.2
port: 80
user: hueuser
device_id: 3

[power huegrp]
type: hue
address: 127.0.0.2
port: 80
user: hueuser
device_id: 1
device_type: group
EOF
done
bash reset_both.sh > /dev/null
~/mr/venv/bin/python power_cmp.py
for i in ref rust; do
    D=~/mr/inst/$i
    mv $D/config/moonraker.conf.bak $D/config/moonraker.conf
done
kill $MOCK
bash reset_both.sh > /dev/null
