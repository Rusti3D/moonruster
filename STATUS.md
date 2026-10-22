# Moonruster status

Goal (user, 2026-10-21): complete Rust port of Moonraker (Arksine/moonraker @ 9e676eba), 100% parity,
drop-in replacement (same config, same database file, same HTTP/WS API).

## State (2026-10-22)
All 37 upstream components are ported and registered (`src/components/mod.rs`). Unit tests: `cargo test` (9 pass).

## Layout
- `ref/moonraker` — reference python source (read-only).
- `src/` — Rust port. Single-threaded tokio (current_thread + LocalSet), Rc/RefCell, `server()` global.
  common.rs, configparser.rs, confighelper.rs, server.rs, loghelper.rs, pyfmt.rs (python str/repr),
  utils.rs, async_serial.rs, sysfs_devs.rs, components/*.rs (one per python component,
  update_manager/ and file_manager/ are folders like upstream).
- Async jinja templates that call server functions (button `call_method`, power http `http_request`)
  render on a worker thread; each call is bridged to the event loop (`template.rs render_async_ext`).

## Test environment (WSL distro `ccs`, root, ~/mr)
- ~/mr/klipper, ~/mr/moonraker (ref, same commit), ~/mr/venv, ~/mr/klipper.dict (tools/build_dict.sh)
- instances ~/mr/inst/ref (python, :7010) and ~/mr/inst/rust (:7020), each with its own batch klippy
- `bash tools/reset_both.sh` restarts both klippys + servers; `python tools/difftest.py` compares 112 cases
- mocks: tools/mock_spoolman.py (:7912), tools/mock_devices.py (:7913 tasmota/shelly/http/wled),
  tools/mock_sp.py (:7914 simplyprint), tools/pdue_test.py (pty PanelDue), tools/setup_um_test.sh (git repos)
- mosquitto installed in WSL for mqtt/sensor/power-mqtt tests
- Build: `CARGO_TARGET_DIR=~/mr/target ~/.cargo/bin/cargo build --release` in /mnt/d/Projects/Moonruster

## Verified against the reference (side by side)
difftest 94/112 identical, remaining diffs are instance values (port, paths, file sizes from paths,
random tokens/uuids), header order, and python set ordering (component list, namespace list).
Compared live: mqtt (3.1.1 + v5, api over mqtt, status publish), sensor, spoolman, update_manager
(git client update/rollback/recover, web client, klipper, PackageKit system packages), power (http
generic w/ templates, klipper_device, mqtt device), notifier list, zeroconf mDNS + SSDP xml, wled http,
paneldue (byte identical serial output), analysis (estimator download/dump), simplyprint (mock cloud).

## Review pass 2026-10-22 (more differential suites, all in tools/)
scenario.py (uploads, metadata for 16 slicer formats + UFP, file ops, prints, history, queue, auth,
octoprint upload; 127/147, rest = random/thumb-size/timing), ext_test.py (agents, unix socket,
klippysocket, subscriptions, db backups; 36/37), power_test.sh + mock_power.py (tplink, tasmota,
shelly, smartthings, homeseer, homeassistant, loxone, hue incl. outgoing requests; 47/47),
machine_test.sh systemd_cli|systemd_dbus (7/8, rest = component order), config_cmp.py (includes,
comments, case, DEFAULT, duplicates, syntax error/backup, secrets; all identical), difftest 117/143.
Run them with tools/run_all.sh (detached: nohup setsid). Pending: mqtt_tls_test.sh (script just
fixed, not run yet), ldap_test.sh + mock_ldap.py (needs /tmp/ldapvenv with ldaptor; not run yet),
re-run scenario/difftest after the latest fixes.

## Not verified (no hardware / service here)
gpio + button + rf + gpio power (no gpiochip in WSL), td1, wled serial, real Spoolman / SimplyPrint
cloud, real smart plugs (mocked only), uhubctl,
ldap, mqtt TLS, apprise delivery (list/config only).

## Known deliberate differences
- Server header "Moonruster/x"; error "traceback" is synthetic.
- LMDB (pre-sqlite) databases not converted; warning added. Restart is via re-exec.
- update_manager "moonraker" entry has type none (status {}): Moonruster is a binary, not a
  python git checkout. Klipper, clients and system updates work.
- notifier delivers through the `apprise` CLI (needs the apprise python package installed).
- preprocess_cancellation runs via system python3; libcamera list is always [];
  thumbnail PNG bytes can differ from PIL.
