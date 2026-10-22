#!/bin/sh
# Moonruster installer for COSMOS (OpenCentauri) printers: BusyBox, root, a
# sysvinit moonraker service and a read-only root filesystem (only /etc is
# writable).  Points the moonraker init script at moonruster; the original
# is backed up and can be restored with -r.
#
# usage: install-cosmos.sh -b <binary> | -r
# Usually run through scripts/remote-install.sh, which picks this script when
# it finds COSMOS.  -s and -d are accepted and ignored.
set -e

INIT=/etc/init.d/moonraker
DEST_DIR=/etc/moonruster
DEST_BIN=$DEST_DIR/moonruster
BACKUP=$DEST_DIR/moonraker.init-backup
BINARY=""
REVERT="n"

while getopts "b:s:d:r" arg; do
    case $arg in
        b) BINARY=$OPTARG;;
        s|d) ;;
        r) REVERT="y";;
        *) exit 1;;
    esac
done

report() { printf '\n###### %s\n' "$1"; }

if [ $REVERT = "y" ]; then
    if [ ! -f "$BACKUP" ]; then
        echo "No backup found at $BACKUP"
        exit 1
    fi
    report "Restoring $INIT"
    $INIT stop
    cp "$BACKUP" "$INIT"
    rm "$BACKUP"
    $INIT start
    echo "Moonraker restored.  $DEST_BIN was left in place."
    exit 0
fi

if [ ! -f "$BINARY" ]; then
    echo "Binary not found: $BINARY"
    exit 1
fi

report "Installing $BINARY to $DEST_BIN"
mkdir -p $DEST_DIR
# replace by rename, a running binary can't be overwritten
cp "$BINARY" "$DEST_BIN.new"
chmod 755 "$DEST_BIN.new"
mv "$DEST_BIN.new" "$DEST_BIN"

if [ ! -f "$BACKUP" ]; then
    report "Backing up $INIT to $BACKUP"
    cp "$INIT" "$BACKUP"
fi

# Always rewritten from the backup.  Moonruster takes moonraker's options;
# only the python interpreter flags are dropped.
report "Pointing $INIT at moonruster"
sed -e "s|^MOONRAKER_EXEC=.*|MOONRAKER_EXEC=\"$DEST_BIN\"|" \
    -e '/^MOONRAKER_ARGS=/{s/-X [^ "]*//g;s/-m moonraker\.moonraker//;s/  */ /g;s/=" /="/;s/ "$/"/}' \
    "$BACKUP" > "$INIT"
grep '^MOONRAKER_' "$INIT"
$INIT restart
report "Done.  Revert with: scripts/remote-install.sh <host> -r"
