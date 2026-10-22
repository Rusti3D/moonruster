#!/bin/bash
# Moonruster installer
#
# Swaps an existing Moonraker systemd service over to Moonruster (keeping
# the same data path, config, database and environment file), or creates a
# new service when Moonraker is not installed.  The original service file is
# backed up and can be restored with -r.
#
# usage: install.sh [-b <binary>] [-s <service name>] [-d <data path>] [-r]
#   -b  moonruster binary to install (default: target/release/moonruster)
#
# Usually run through scripts/remote-install.sh from a PC, which copies the
# binary and this script to the printer.
#   -s  service name (default: moonraker)
#   -d  data path for a new install (default: ~/printer_data)
#   -r  revert: restore the backed up Moonraker service file
set -e

SRCDIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )/.." && pwd )"
BINARY="${SRCDIR}/target/release/moonruster"
SERVICE="moonraker"
DATA_PATH="${HOME}/printer_data"
REVERT="n"
DEST_BIN="/usr/local/bin/moonruster"

while getopts "b:s:d:r" arg; do
    case $arg in
        b) BINARY=$OPTARG;;
        s) SERVICE=$OPTARG;;
        d) DATA_PATH=$OPTARG;;
        r) REVERT="y";;
        *) exit 1;;
    esac
done

SERVICE_FILE="/etc/systemd/system/${SERVICE}.service"
BACKUP_FILE="${SERVICE_FILE}.moonraker-backup"

report() { echo -e "\n###### $1"; }

if [ "$(id -u)" = "0" ]; then
    echo "Run this script as the user that runs Moonraker, not as root"
    exit 1
fi

if [ $REVERT = "y" ]; then
    if [ ! -f "$BACKUP_FILE" ]; then
        echo "No backup found at $BACKUP_FILE"
        exit 1
    fi
    report "Restoring $SERVICE_FILE"
    sudo systemctl stop "$SERVICE"
    sudo mv "$BACKUP_FILE" "$SERVICE_FILE"
    sudo systemctl daemon-reload
    sudo systemctl start "$SERVICE"
    echo "Moonraker restored.  $DEST_BIN was left in place."
    exit 0
fi

if [ ! -f "$BINARY" ]; then
    echo "Binary not found: $BINARY"
    echo "Pass one with -b, or install from a PC with scripts/remote-install.sh"
    exit 1
fi

report "Installing $BINARY to $DEST_BIN"
sudo install -m 755 "$BINARY" "$DEST_BIN"

if [ -f "$SERVICE_FILE" ]; then
    # Existing install: only ExecStart changes.  The environment file keeps
    # MOONRAKER_ARGS ("-m moonraker" or a path to moonraker.py), which
    # moonruster accepts.
    if [ ! -f "$BACKUP_FILE" ]; then
        report "Backing up $SERVICE_FILE to $BACKUP_FILE"
        sudo cp "$SERVICE_FILE" "$BACKUP_FILE"
    fi
    report "Pointing $SERVICE at moonruster"
    sudo systemctl stop "$SERVICE"
    sudo sed -i "s|^ExecStart=.*|ExecStart=${DEST_BIN} \$MOONRAKER_ARGS|" "$SERVICE_FILE"
    sudo systemctl daemon-reload
    sudo systemctl start "$SERVICE"
    report "Done.  Revert with: $0 -r -s $SERVICE"
    exit 0
fi

# New install
report "Creating data path ${DATA_PATH}"
mkdir -p "${DATA_PATH}"/{config,logs,gcodes,systemd,comms,database}
ENV_FILE="${DATA_PATH}/systemd/moonraker.env"
if [ ! -f "$ENV_FILE" ]; then
    printf 'MOONRAKER_DATA_PATH="%s"\nMOONRAKER_ARGS="-m moonraker"\n' "$DATA_PATH" > "$ENV_FILE"
fi
if [ ! -f "${DATA_PATH}/config/moonraker.conf" ]; then
    cat > "${DATA_PATH}/config/moonraker.conf" <<EOF
[server]
host: 0.0.0.0
port: 7125
klippy_uds_address: ${DATA_PATH}/comms/klippy.sock

[authorization]
trusted_clients:
    10.0.0.0/8
    127.0.0.0/8
    169.254.0.0/16
    172.16.0.0/12
    192.168.0.0/16
    FE80::/10
    ::1/128
cors_domains:
    *.lan
    *.local
    *://localhost
    *://localhost:*
    *://my.mainsail.xyz
    *://app.fluidd.xyz

[octoprint_compat]

[history]
EOF
fi

report "Creating ${SERVICE_FILE}"
sudo groupadd -f moonraker-admin
sudo /bin/sh -c "cat > ${SERVICE_FILE}" <<EOF
# systemd service file for moonraker
[Unit]
Description=API Server for Klipper SV1
Requires=network-online.target
After=network-online.target

[Install]
WantedBy=multi-user.target

[Service]
Type=simple
User=$USER
SupplementaryGroups=moonraker-admin
RemainAfterExit=yes
EnvironmentFile=${ENV_FILE}
ExecStart=${DEST_BIN} \$MOONRAKER_ARGS
Restart=always
RestartSec=10
EOF

# PolicyKit rules (same as moonraker's set-policykit-rules.sh) so service
# control, reboot/shutdown and system updates work without sudo
if command -v pkaction > /dev/null; then
    PK_VER="$( pkaction --version | grep -Po "(\d+\.?\d*)" )"
    ACTIONS="org.freedesktop.systemd1.manage-units;org.freedesktop.login1.power-off"
    ACTIONS="${ACTIONS};org.freedesktop.login1.power-off-multiple-sessions;org.freedesktop.login1.reboot"
    ACTIONS="${ACTIONS};org.freedesktop.login1.reboot-multiple-sessions;org.freedesktop.login1.halt"
    ACTIONS="${ACTIONS};org.freedesktop.login1.halt-multiple-sessions;org.freedesktop.packagekit.*"
    if [ "$PK_VER" = "0.105" ]; then
        report "Installing PolicyKit rules (legacy)"
        sudo /bin/sh -c "cat > /etc/polkit-1/localauthority/50-local.d/10-moonraker.pkla" <<EOF
[moonraker permissions]
Identity=unix-user:$USER
Action=$ACTIONS
ResultAny=yes
EOF
    else
        RULE_DIR="/usr/share/polkit-1/rules.d"
        [ -d $RULE_DIR ] || RULE_DIR="/etc/polkit-1/rules.d"
        GID=$( getent group moonraker-admin | awk -F: '{printf "%d", $3}' )
        report "Installing PolicyKit rules to ${RULE_DIR}/moonraker.rules"
        sudo /bin/sh -c "cat > ${RULE_DIR}/moonraker.rules" <<EOF
// Allow Moonraker User to manage systemd units, reboot and shutdown
// the system
polkit.addRule(function(action, subject) {
    if ((action.id == "org.freedesktop.systemd1.manage-units" ||
         action.id == "org.freedesktop.login1.power-off" ||
         action.id == "org.freedesktop.login1.power-off-multiple-sessions" ||
         action.id == "org.freedesktop.login1.reboot" ||
         action.id == "org.freedesktop.login1.reboot-multiple-sessions" ||
         action.id == "org.freedesktop.login1.halt" ||
         action.id == "org.freedesktop.login1.halt-multiple-sessions" ||
         action.id.startsWith("org.freedesktop.packagekit.")) &&
        subject.user == "$USER") {
        // Only allow processes with the "moonraker-admin" supplementary group
        // access
        var regex = "^Groups:.+?\\\\s$GID[\\\\s\\\\0]";
        var cmdpath = "/proc/" + subject.pid.toString() + "/status";
        try {
            polkit.spawn(["grep", "-Po", regex, cmdpath]);
            return polkit.Result.YES;
        } catch (error) {
            return polkit.Result.NOT_HANDLED;
        }
    }
});
EOF
    fi
else
    echo "PolicyKit not installed, service control and reboot will need sudo"
fi

sudo systemctl daemon-reload
sudo systemctl enable "$SERVICE"
sudo systemctl start "$SERVICE"
report "Done.  Moonruster is running as ${SERVICE}.service on port 7125"
