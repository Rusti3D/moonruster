#!/bin/bash
# Installs Moonruster on a printer over SSH, run from a PC.  Only the binary
# and install.sh are copied to the printer (into /tmp); nothing is built there.
#
# usage: remote-install.sh <user@host> [-b <binary>] [-s <service>] [-d <data path>] [-r]
#   -b  binary to install (default: dist/moonruster-linux-<arch>, else the
#       latest GitHub release for the printer's architecture)
#   -s  service name (default: moonraker)
#   -d  data path for a new install (default: ~/printer_data on the printer)
#   -r  revert the printer to Moonraker
set -e

HOST=$1
if [ -z "$HOST" ] || [ "${HOST:0:1}" = "-" ]; then
    echo "usage: $0 <user@host> [-b <binary>] [-s <service>] [-d <data path>] [-r]"
    exit 1
fi
shift
SRCDIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )/.." && pwd )"
REPO="shawn-makes-stuff/moonruster"
BINARY=""
ARGS=()
REVERT="n"
while getopts "b:s:d:r" arg; do
    case $arg in
        b) BINARY=$OPTARG;;
        s) ARGS+=(-s "$OPTARG");;
        d) ARGS+=(-d "$OPTARG");;
        r) REVERT="y"; ARGS+=(-r);;
        *) exit 1;;
    esac
done

# one connection for all commands, so the password is asked once; accept-new
# because the host key prompt has no usable tty when run from Windows
CTL="/tmp/moonruster-ssh-$$"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ControlMaster=auto -o ControlPath="$CTL" -o ControlPersist=60)
trap 'ssh "${SSH_OPTS[@]}" -O exit "$HOST" 2>/dev/null || true' EXIT

ssh "${SSH_OPTS[@]}" "$HOST" "cat > /tmp/moonruster-install.sh" < "$SRCDIR/scripts/install.sh"

if [ $REVERT = "n" ]; then
    MACH=$(ssh "${SSH_OPTS[@]}" "$HOST" uname -m)
    case $MACH in
        aarch64|arm64) ARCH=aarch64;;
        armv7l|armv8l) ARCH=armv7;;
        armv6l) ARCH=armv6;;
        x86_64|amd64) ARCH=x86_64;;
        *) echo "Unsupported printer architecture: $MACH"; exit 1;;
    esac
    echo "Printer architecture: $MACH ($ARCH)"
    if [ -z "$BINARY" ]; then
        BINARY="$SRCDIR/dist/moonruster-linux-$ARCH"
        if [ ! -f "$BINARY" ]; then
            BINARY="$(mktemp)"
            URL="https://github.com/$REPO/releases/latest/download/moonruster-linux-$ARCH"
            echo "Downloading $URL"
            if ! curl -fL -o "$BINARY" "$URL"; then
                echo "No local build or release binary found.  Build one with tools/cross_build.sh"
                exit 1
            fi
        fi
    fi
    echo "Copying $BINARY to $HOST"
    ssh "${SSH_OPTS[@]}" "$HOST" "cat > /tmp/moonruster" < "$BINARY"
    ARGS+=(-b /tmp/moonruster)
fi

# -t: sudo may ask for the password on the printer
REMOTE_ARGS=$(printf '%q ' "${ARGS[@]}")
ssh "${SSH_OPTS[@]}" -t "$HOST" "bash /tmp/moonruster-install.sh $REMOTE_ARGS; rm -f /tmp/moonruster /tmp/moonruster-install.sh"
