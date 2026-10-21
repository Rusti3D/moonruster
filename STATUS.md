# Moonruster status

Goal (user, 2026-10-21): complete Rust port of Moonraker (Arksine/moonraker @ 9e676eba), 100% parity,
drop-in replacement (same config, same database file, same HTTP/WS API). User asleep, full autonomy.

## Layout
- `ref/moonraker` — reference python source (read-only). Port file by file from it.
- `src/` — Rust port. Single-threaded tokio (current_thread + LocalSet), Rc/RefCell, `server()` global.
  - common.rs (WebRequest, ApiDefinition, JsonRpc), configparser.rs (python configparser emulation),
    confighelper.rs, server.rs, loghelper.rs, pyfmt.rs (python str/repr), utils.rs
  - components/*.rs one per python component.
- `tools/` — WSL test scripts. Build & test in WSL distro `ccs` (user root, ~/mr):
  - ~/mr/klipper (latest), ~/mr/moonraker (ref, same commit), ~/mr/venv (python deps)
  - ~/mr/klipper.dict built for atmega2560 via tools/build_dict.sh (test asset dict is too old)
  - instances: ~/mr/inst/ref (python, port 7010) and ~/mr/inst/rust (port 7020), each own klippy
  - tools/setup_instance.sh, start_klippy.sh <name>, start_ref.sh
  - Build: `CARGO_TARGET_DIR=~/mr/target ~/.cargo/bin/cargo build` from /mnt/d/Projects/Moonruster

## Component port status
Done (written, not yet compiled): application, websockets, klippy_connection, klippy_apis, database,
template, secrets, authorization, server, confighelper, loghelper.
TODO (port fully from ref): file_manager (+metadata, upload), machine, data_store, shell_command,
proc_stats, job_state, job_queue, history, http_client, announcements, webcam, extensions,
dbus_manager, octoprint_compat, power, update_manager, mqtt, zeroconf, sensor, spoolman,
simplyprint, paneldue, wled, gpio, button, ldap, analysis, td1, notifier.

## Known deliberate differences
- Server header "Moonruster/x", error "traceback" is synthetic (python traceback impossible).
- LMDB (pre-sqlite) databases not converted; warning added.
- Restart is in-process via re-exec.
