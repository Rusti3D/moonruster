# Installing Moonruster

Moonruster is a single static binary, installed at `/usr/local/bin/moonruster` on the printer host.
Nothing is compiled on the printer: you install it from a PC over SSH, and only the binary and a
small install script are copied over.

Requirements on the printer: Linux with systemd (Raspberry Pi OS, Armbian, Debian/Ubuntu based
printer images), SSH access, and a user with sudo (usually `pi`).

## 1. Get the binary (on your PC)

Either download it from the [releases page](https://github.com/shawn-makes-stuff/moonruster/releases)
(the install script does this for you), or build it yourself. Building needs Linux or WSL:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf \
    arm-unknown-linux-musleabihf x86_64-unknown-linux-musl
pip install ziglang cargo-zigbuild
git clone https://github.com/shawn-makes-stuff/moonruster.git
cd moonruster
bash tools/cross_build.sh
```

This puts one binary per architecture in `dist/`:

| File | For |
| --- | --- |
| `moonruster-linux-aarch64` | 64-bit Raspberry Pi OS (Pi 3/4/5, CM4), most 64-bit SBCs |
| `moonruster-linux-armv7` | 32-bit Raspberry Pi OS on Pi 2/3/4 |
| `moonruster-linux-armv6` | Pi Zero / Pi 1 |
| `moonruster-linux-x86_64` | PCs, mini PCs |

The binaries are static, so they don't depend on the printer's library versions.

## 2a. Replace an existing Moonraker

From the `moonruster` folder on your PC (Linux, macOS, WSL or Git Bash on Windows):

```bash
scripts/remote-install.sh pi@printer.local
```

It detects the printer's architecture, copies the matching binary from `dist/` (or downloads it
from the latest release), and on the printer:

- installs the binary to `/usr/local/bin/moonruster`
- backs up `/etc/systemd/system/moonraker.service` to `moonraker.service.moonraker-backup`
- changes only the `ExecStart=` line to `/usr/local/bin/moonruster $MOONRAKER_ARGS`
- restarts the service

You may be asked for the SSH password and then the sudo password on the printer.

Nothing else changes: `~/printer_data`, `moonraker.conf`, the database, the environment file
(`printer_data/systemd/moonraker.env`), PolicyKit rules and the `moonraker-admin` group stay as
they are, and the Moonraker folder and its virtualenv are not touched. For a second instance pass
its service name: `scripts/remote-install.sh pi@printer.local -s moonraker-2`.

Check it is up: open Mainsail/Fluidd as usual, or `curl http://printer.local:7125/server/info`.
Logs stay at `~/printer_data/logs/moonraker.log` on the printer.

### Going back to Moonraker

```bash
scripts/remote-install.sh pi@printer.local -r
```

restores the backed up service file and restarts Moonraker. The database format is the same, so
history, settings and users carry over in both directions. `/usr/local/bin/moonruster` is left in
place; delete it if you want the space back.

## 2b. New install (no Moonraker on the printer)

```bash
scripts/remote-install.sh pi@printer.local -d /home/pi/printer_data
```

Use the full path (a `~` would expand on your PC). This creates the data folder with a basic
`moonraker.conf`, the environment file, `moonraker.service`, the `moonraker-admin` group and the
same PolicyKit rules Moonraker installs (needed for service control, reboot/shutdown and system
updates without sudo). Klipper must be started with `-a <data path>/comms/klippy.sock` (the
default for KIAUH installs).

## Installing without the PC script

Copy a binary and `scripts/install.sh` to the printer any way you like, then on the printer:

```bash
bash install.sh -b ./moonruster-linux-aarch64      # or -r to revert
```

## Notifications (optional)

`[notifier ...]` sections need the Apprise command line tool on the printer:

```bash
sudo apt install apprise
```

Moonruster runs `apprise` (or `python3 -m apprise`), so it must be installed system wide. Nothing
else needs Python.

## Updating

From Mainsail/Fluidd: the update manager's `moonraker` entry shows the installed Moonruster version
against the latest [release](https://github.com/shawn-makes-stuff/moonruster/releases). Updating
downloads the binary for the printer's architecture, replaces the installed one (with sudo when
its folder isn't writable, e.g. `/usr/local/bin`) and restarts the service.

Or build/download the new binary and run `scripts/remote-install.sh` again.
