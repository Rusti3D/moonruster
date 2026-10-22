#!/bin/bash
# Compares the machine component of both servers with a systemd provider.
# Installs a dummy "mrtest" service (sleep), switches [machine] provider in
# both test instances, runs the comparison, then restores everything.
# usage: machine_test.sh <systemd_cli|systemd_dbus>
PROV=${1:-systemd_cli}
cd "$(dirname "$0")"
cat > /etc/systemd/system/mrtest.service <<'EOF'
[Unit]
Description=Moonruster test dummy
[Service]
ExecStart=/bin/sleep infinity
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl start mrtest
for i in ref rust; do
    D=~/mr/inst/$i
    cp $D/config/moonraker.conf $D/config/moonraker.conf.bak
    cp $D/moonraker.asvc $D/moonraker.asvc.bak
    printf "mrtest\n" >> $D/moonraker.asvc
    sed -i "s/^provider: none/provider: $PROV\nvalidate_service: False\nvalidate_config: False/" $D/config/moonraker.conf
done
bash reset_both.sh > /dev/null
~/mr/venv/bin/python machine_cmp.py
for i in ref rust; do
    D=~/mr/inst/$i
    mv $D/config/moonraker.conf.bak $D/config/moonraker.conf
    mv $D/moonraker.asvc.bak $D/moonraker.asvc
done
systemctl stop mrtest
rm /etc/systemd/system/mrtest.service
systemctl daemon-reload
bash reset_both.sh > /dev/null
