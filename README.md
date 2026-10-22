# Moonruster

A Rust port of [Moonraker](https://github.com/Arksine/moonraker), the API server for
[Klipper](https://github.com/Klipper3d/klipper). It is meant as a drop-in replacement: it reads the
same `moonraker.conf`, uses the same database file and data folder, and serves the same HTTP,
websocket, unix socket and MQTT APIs, so Mainsail, Fluidd, KlipperScreen, slicer uploads and other
Moonraker clients work without changes.

Ported from Moonraker at commit `9e676eba` (reports itself as `v0.11.0-3-g9e676eb`, API 1.5.0).

## Status

All 37 Moonraker components are ported. Behaviour is checked by running Moonraker and Moonruster
side by side against the same Klipper setup and diffing every response and notification (see
`tools/` and `STATUS.md`).

Not yet verified on real hardware: GPIO, buttons, RF/GPIO power, TD-1, WLED over serial, uhubctl,
real smart plugs (only mocked), Spoolman and SimplyPrint cloud services.

Known differences from Moonraker:

- The update manager cannot update Moonruster itself (the `moonraker` entry is empty). Klipper,
  web clients, other repos and system packages update normally.
- Thumbnails generated for the 32x32 preview can differ by a few bytes from Pillow's output.
- Pre-SQLite (LMDB) databases are not converted.
- The `Server` header reports `Moonruster/<version>`.

## Python

The server itself does not need Python. Two optional features call out to Python packages, the same
ones Moonraker uses:

| Feature | Needs | Only when |
| --- | --- | --- |
| Notifications (`[notifier ...]`) | `apprise` command (`pip install apprise`) | a notifier section is configured |
| Object processing for cancel-object | `preprocess-cancellation` | `enable_object_processing: True` in `[file_manager]` |

Without them those two features log an error and everything else runs normally.

## Install

See [docs/INSTALL.md](docs/INSTALL.md). Short version, on the printer host:

```bash
git clone https://github.com/shawn-makes-stuff/moonruster.git
cd moonruster
cargo build --release
./scripts/install.sh          # swaps an existing moonraker.service over
./scripts/install.sh -r       # goes back to Moonraker
```

## Command line

Same options and environment variables as Moonraker:

```
moonruster [-d <data path>] [-c <configfile>] [-l <logfile>] [-u <unixsocket>] [-n] [-s] [-v] [-g] [-o]
```

`MOONRAKER_DATA_PATH`, `MOONRAKER_CONFIG_PATH`, `MOONRAKER_LOG_PATH`, `MOONRAKER_UDS_PATH`,
`MOONRAKER_DISABLE_FILE_LOG`, `MOONRAKER_VERBOSE_LOGGING` and the rest work the same way.

## License

GPL-3.0, as Moonraker.
