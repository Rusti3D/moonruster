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

- The update manager's `moonraker` entry updates Moonruster from this repo's GitHub releases
  (the binary for its architecture, then a service restart) instead of a git checkout.
- Thumbnails generated for the 32x32 preview can differ by a few bytes from Pillow's output.
- Pre-SQLite (LMDB) databases are not converted.
- The `Server` header reports `Moonruster/<version>`.

## Performance

Synthetic benchmarks, Moonraker `9e676eba` (Python 3.13) vs Moonruster, same config, x86_64.
"Own memory" is the process's anonymous memory (`Pss_Anon`); code mapped from the program files
(about 15 MB for either) is left out because the kernel can drop it under memory pressure.

With a fake Klipper streaming realistic print status (toolhead, extruder, bed, motion report...)
to websocket clients subscribed like Mainsail, 30 s per run:

| | Moonraker | Moonruster |
| --- | --- | --- |
| Idle, CPU | 0.08 s | 0.03 s |
| 20 status updates/s, 2 clients, CPU | 0.57 s | 0.22 s |
| Own memory | 35 MB | 2.1 MB |

With 200 G-code files and no Klipper:

| | Moonraker | Moonruster |
| --- | --- | --- |
| Startup CPU | 1.84 s | 0.39 s |
| CPU for 3,500 API requests | 28.4 s | 2.6 s |
| `/server/files/list` (200 files) | 18 req/s | 212 req/s |

On a printer (Elegoo Centauri Carbon, armv7, 115 MB RAM, COSMOS firmware whose Moonraker already
uses lazy imports) Moonraker's own memory was 22.0 MB (19.8 MB in RAM + 2.2 MB in zram swap) and
Moonruster's 6.8 MB (5.6 + 1.2), leaving 10.3 MB more `MemAvailable` and half the zram use. That
was before routes stopped being compiled as regexes, which cut Moonruster's own memory by about
1.3 MB in the synthetic test. The real-world gap is about 3x rather than the 10x+ above: a real
Klipper, file list, history and webcam add data on both sides, and COSMOS trims its Python.

Reproduce the first table with `tools/bench.sh` (it starts `tools/mock_klippy.py` and
`tools/ws_clients.py`, which needs `tornado`, e.g. from Moonraker's virtualenv):

```bash
PY=~/moonraker-env/bin/python tools/bench.sh target/release/moonruster
PY=~/moonraker-env/bin/python tools/bench.sh ~/moonraker-env/bin/python ~/moonraker/moonraker/moonraker.py
RATE=0.001 CLIENTS=0 tools/bench.sh target/release/moonruster    # idle
```

On a printer, compare the running service's own memory with
`grep -e Pss_Anon -e SwapPss /proc/$(pidof moonruster)/smaps_rollup` (or the Moonraker pid).

## Python

Moonruster does not need Python. Object processing for cancel-object (`enable_object_processing`)
is built in, a port of the `preprocess-cancellation` package that produces byte-identical output.

The one exception is notifications: `[notifier ...]` sections deliver through the `apprise` command
(`sudo apt install apprise`), the same Apprise library Moonraker uses. Without it notifications
log an error and everything else runs normally.

## Install

See [docs/INSTALL.md](docs/INSTALL.md). Nothing is built on the printer: from a PC, with the
repo checked out,

```bash
scripts/remote-install.sh pi@printer.local       # swaps an existing moonraker.service over
scripts/remote-install.sh pi@printer.local -r    # goes back to Moonraker
```

copies a static binary for the printer's architecture (from `dist/`, built with
`tools/cross_build.sh`, or the latest release) and swaps the service over SSH.

## Command line

Same options and environment variables as Moonraker:

```
moonruster [-d <data path>] [-c <configfile>] [-l <logfile>] [-u <unixsocket>] [-n] [-s] [-v] [-g] [-o]
```

`MOONRAKER_DATA_PATH`, `MOONRAKER_CONFIG_PATH`, `MOONRAKER_LOG_PATH`, `MOONRAKER_UDS_PATH`,
`MOONRAKER_DISABLE_FILE_LOG`, `MOONRAKER_VERBOSE_LOGGING` and the rest work the same way.

## License

GPL-3.0, as Moonraker.
