# Installing Moonruster

Moonruster runs on Linux (it uses inotify, unix sockets and systemd). It is installed as a single
binary at `/usr/local/bin/moonruster`.

## 1. Build

On the printer host (Raspberry Pi or similar):

```bash
sudo apt install build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
git clone https://github.com/shawn-makes-stuff/moonruster.git
cd moonruster
cargo build --release
```

The first build takes a while on a Pi (SQLite is compiled in). The result is
`target/release/moonruster`. Cross compiling from a PC is possible but has not been tested yet.

## 2a. Replace an existing Moonraker

Run as the user that runs Moonraker (usually `pi`), not as root:

```bash
./scripts/install.sh
```

This:

- copies the binary to `/usr/local/bin/moonruster`
- backs up `/etc/systemd/system/moonraker.service` to `moonraker.service.moonraker-backup`
- changes only the `ExecStart=` line to `/usr/local/bin/moonruster $MOONRAKER_ARGS`
- restarts the service

Nothing else changes: `~/printer_data`, `moonraker.conf`, the database, the environment file
(`printer_data/systemd/moonraker.env`), PolicyKit rules and the `moonraker-admin` group stay as
they are. Your Moonraker folder and its virtualenv are not touched.

For a second instance use its service name, for example `./scripts/install.sh -s moonraker-2`.

Check it is up:

```bash
systemctl status moonraker
curl -s localhost:7125/server/info
```

Logs go to the same place as before, `~/printer_data/logs/moonraker.log`.

### Going back to Moonraker

```bash
./scripts/install.sh -r
```

restores the backed up service file and restarts Moonraker. The database format is the same, so
history, settings and users carry over in both directions.

## 2b. New install (no Moonraker)

```bash
./scripts/install.sh -d ~/printer_data
```

This creates `~/printer_data` with a basic `moonraker.conf`, the environment file, the
`moonraker.service` unit, the `moonraker-admin` group and the same PolicyKit rules Moonraker
installs (needed for service control, reboot/shutdown and system updates without sudo). Klipper
must be started with `-a ~/printer_data/comms/klippy.sock` (the default for KIAUH installs).

## Optional Python features

```bash
sudo apt install apprise                                              # [notifier] sections
sudo pip3 install preprocess-cancellation --break-system-packages    # enable_object_processing: True
```

Moonruster runs the `apprise` command (or `python3 -m apprise`) and the system `python3` for
object processing, so both must be installed system wide, not in a user-only or pipx location the
service can't see. Both are only needed for those features.

## Updating

```bash
cd ~/moonruster
git pull
cargo build --release
./scripts/install.sh
```

The update manager in Mainsail/Fluidd does not update Moonruster itself yet.
