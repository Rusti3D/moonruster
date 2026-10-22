# Next steps (handoff)

Where the project stands and what to do next. Read `STATUS.md` for the code layout and
verification history, `README.md` / `docs/INSTALL.md` for users.

## State (2026-10-22, commit after 104133f)

- All 37 Moonraker components are ported (Moonraker commit `9e676eba`).
- Verified side by side against the Python reference with the differential suites in `tools/`.
  Last results (remaining diffs are random tokens, python `set` ordering of components /
  namespaces / sections, instance names, timing races that also vary upstream):
  - `scenario.py` 127/147, `ext_test.py` 36/37, power (`power_test.sh`) 47/47,
    machine `systemd_cli` / `systemd_dbus` 7/8, `config_cmp.py` all identical, `difftest.py` 117/143.
- `scripts/install.sh` swaps an existing `moonraker.service` to Moonruster (backup + `-r` revert)
  or creates a new install. Only syntax checked, never run on a real host.

## Rebuilding the test environment

`ref/` (the reference Moonraker source) is gitignored. The suites expect:

- Linux with systemd (they were run in WSL Ubuntu 24.04, as root, under `~/mr`).
- `~/mr/moonraker`: Moonraker checked out at `9e676eba`; `~/mr/venv` with its requirements
  (`scripts/moonraker-requirements.txt`) plus `requests`.
- `~/mr/klipper` and `~/mr/klipper.dict` (`tools/build_dict.sh`), batch mode klippy, no MCU.
- Instances `~/mr/inst/ref` (python, port 7010) and `~/mr/inst/rust` (port 7020) from
  `tools/setup_instance.sh <name> <port>`; the current configs also include mqtt (mosquitto on
  1883), spoolman/simplyprint/device mocks, paneldue, etc. Mocks: `tools/mock_*.py`.
- Build: `CARGO_TARGET_DIR=~/mr/target cargo build --release`.
- `bash tools/reset_both.sh` restarts both klippys and servers.
- Run suites detached: `nohup setsid bash tools/run_all.sh [suite...] < /dev/null &`
  (suites: power machine_cli machine_dbus mqtt_tls ldap ppc config scenario ext difftest), results in
  `/tmp/mr_<suite>.out`. Running them attached to a terminal stopped every process (SIGTTIN).
- Batch mode klippy hangs after `emergency_stop` / `firmware_restart`, so those are not tested.

## To do, in order

Done since the handoff: scenario 122/147 and difftest 118/143 re-run on the latest build (all
remaining diffs are noise, the copy notification order now matches), MQTT over TLS identical,
LDAP 9/9 (`ldap_test.sh`), `preprocess_cancellation` ported to Rust
(`src/components/file_manager/cancellation.rs`), byte-identical on 6 files (`ppc_test.sh`).
Apprise is now the only (optional) Python dependency.

Printer installs need no build on the printer: `tools/cross_build.sh` (cargo-zigbuild) makes
static musl binaries for aarch64, armv7, armv6 and x86_64 in `dist/` (~17 MB each), and
`scripts/remote-install.sh user@host` copies the right one plus `install.sh` over SSH and swaps
the service. `.github/workflows/release.yml` builds the binaries for `v*` tags (not run yet, no
release published). The aarch64 binary passed difftest/ext/ppc under qemu-user
(`MOONRUSTER_BIN=dist/... tools/run_all.sh ...`); only qemu artifacts differ (CPU info, PolicyKit
start-time check).

1. **Test the installer** (`scripts/remote-install.sh`) against a real or throwaway Debian host with
   an existing Moonraker (KIAUH install): swap, check Mainsail/Fluidd, revert with `-r`, check
   history/settings survive. Then tag `v0.1.0` to publish release binaries.
2. **First real printer test** (ideally a spare printer): Mainsail/Fluidd, uploads from a slicer,
   printing, update manager, webcam, power devices.
3. **Untestable here, needs hardware**: GPIO, buttons, RF/GPIO power, TD-1, WLED serial,
   uhubctl, real smart plugs, Spoolman and SimplyPrint services.
4. **Optional**: a self-update path for Moonruster in the update manager (its `moonraker` entry
   is type none now); test cross compiling for aarch64/armv7 and publish release binaries.

## Known, deliberate differences

- Update manager cannot update Moonruster itself.
- Thumbnail PNG bytes differ slightly from Pillow (zlib build dependent, can't match exactly).
- LMDB (pre-SQLite) databases are not converted.
- `/server/notifiers/list` with an `attach` option returns the template source; upstream fails
  with a 500 (JSON encoding of a template object), that bug is not copied.
- `Server` header is `Moonruster/<version>`; error tracebacks are synthetic.
- Server restart re-execs the binary (upstream restarts in process).
