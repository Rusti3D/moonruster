# Rustify

Moonruster is Moonraker ported to Rust. It keeps Moonraker's logic, and also Moonraker's (Python's) shape: a dynamic
object model that Python gives Moonraker, rebuilt by hand in Rust. Rustify keeps the logic and changes the shape.
Components, events, requests, config and state become Rust's own types, traits and ownership.

This plan follows kaliru's (Rusti3D/kaliru, branch `rustify`, `docs/RUSTIFY.md`): same rules, same gates, same
conventions where the code is alike. The plugin layer under `src/plugins` is shared with kaliru: its interface changes
only by agreement with kaliru.

## Rules

1. **It cannot break.** Nothing visible from outside changes:
   - the HTTP and websocket JSON (key order included);
   - JSON-RPC errors;
   - MQTT;
   - the log lines;
   - files written (database, history, metadata, uploads);
   - config error messages;
   - the Python component API.

   Every commit passes all the parity harnesses (`tools/rustify_gate.sh`).
2. **Resource usage only gets better.** Every commit is measured against the baseline: CPU time, peak RSS, anonymous
   PSS and binary size (`tools/resource_gate.sh`). A regression beyond the noise does not land. Milestones are also
   measured on the CC1.
3. **Crates where they give a real, measured gain**, checked against binary size and memory.
4. **Small steps.** One change per commit, both gates green, no big-bang rewrite.
5. **Python semantics only at the edges.** `pyfmt` (Python's `str` / `repr` / float formatting), Python exception
   classes and `ServerError` texts stay where they are output: the API, the logs, Jinja, plugins. Inside, Rust types.

## Where Moonruster stands (main 0f7a56e: one crate, 96 files, 52k lines)

| Python's shape, rebuilt in Rust | count | cost |
|---|---|---|
| `serde_json::Value` / `json!` / `Map<String, Value>` | 1,473 / 951 / 200 | Untyped data everywhere: status, config, requests, results, events. Missing keys and wrong types are runtime errors. A value tree is built for every response. |
| `RefCell` fields, `.borrow()` / `.borrow_mut()` | 294 / 710 / 780 (+163 `Cell`) | A runtime flag per field. Runtime panics instead of compile errors. |
| string-named events, `Vec<Value>` payloads | ~50 send sites, ~40 handlers | Unchecked payloads, positional `args[0]` access, a JSON round trip for Rust-to-Rust events. |
| `lookup_component::<T>("name")`, `Rc<dyn Any>` table | 127 / 13 `component_any` | A string key plus a downcast for components that always exist. |
| config getters (`get` / `getint` / `getboolean` / ...) | 1,575 | Hand-written parsing in every component. |
| request args: `WebRequest` over `Map<String, Value>`, `get_str` / `get_int` ... | (in the endpoints) | Every endpoint parses its arguments by hand. |
| `.clone()` / `format!` | 1,255 / 1,136 | Allocations (`String` keys, cloned maps). |
| `spawn` + `JoinHandle` kept in `RefCell<Option<..>>` | 125 | Task lifetimes managed by hand (abort on drop or close). |
| `unsafe` | 152 | ioctl/libc (gpio, sysfs, inotify, CPython C API). No SAFETY notes. |
| `#[test]` | 15 | Correctness lives only in the external parity harnesses. |

Largest modules: `machine` 2.4k lines, `power` 2.3k, `simplyprint` 1.9k, `application` 1.9k, `update_manager` 1.7k,
`file_manager` 1.6k (+1.4k metadata, +1.2k observer), `confighelper` 1.5k.

Moonruster is already async (tokio, current-thread). Its rustify is about structure, not the execution model. The
reverse of kaliru's case, where greenlets are the question.

## Gates

**`tools/rustify_gate.sh`** runs every harness and diffs their summaries against `tools/rustify_gate.expected`,
recorded on main:
- `difftest`, `scenario`, `power`, `machine_cli` / `machine_dbus`, `mqtt_tls`, `config`, `ext`, `ldap`, `ppc`;
- `plugin_cmp`, `corpus_cmp`;
- `mirror_check`.

What it compares and how:
- **Per case, not counts.** The gate compares the set of differing cases and their diffs, not the totals.
- **Volatile order made unordered.** A case that differs only by an order Moonraker itself randomises (component load
  order: Python set order) is compared unordered in the harness, as `scenario` already does. It is then no longer a
  difference.
- **No sampling of timing flakes.** Moonraker's own timing races (file-event spill-over, a late pipe callback) are
  made deterministic in the test (wait for quiescence).
- **Expected = main.** The expected file is main's output. A rustify commit must reproduce it exactly.

**`tools/resource_gate.sh`** compares against a stored baseline (min of 5 runs, tolerance from the baseline's spread):
- Idle and load runs:
  - load is a mock Klippy at 20 status updates/s, with 2 websocket clients subscribed like Mainsail (as
    `cc1_plugins_measure.sh`);
  - each run gives CPU from `/proc/<pid>/task/*/schedstat` (ns), peak RSS, and anonymous PSS.
- The same with a Python component loaded (`tools/plugin_mem.py` corpus).
- Startup time to "Server Ready".
- Binary sizes: x86-64 and armv7 (glibc, non-PIE, as shipped).

CC1 A/B (`tools/cc1_plugins_measure.sh`) is run at each milestone.

## Phases

**0. Guardrails**
- `rustify_gate.sh` with `rustify_gate.expected` recorded on main, after making the volatile cases deterministic.
- `resource_gate.sh` and its baseline.
- Unit tests for the pure modules:
  - `pyfmt` (Python's float `repr`, `str` of containers);
  - `configparser`;
  - `pymimetypes`;
  - the gcode metadata parsers;
  - `shlex`, `cleandoc`, base64url (binascii's non-strict decoder);
  - JSON-RPC parsing.
- Clippy baseline; a SAFETY note on each `unsafe`.

*Phase 0 status (2026-10-25)*
- **Gates are on main:**
  - `tools/rustify_gate.expected` recorded at 21b74f2. The summary was identical over repeated runs once
    `scenario` waited for quiescence.
  - `tools/resource_gate.baseline` recorded.
  - Both gates, and every build, take `/tmp/rustify_gates.lock`, shared with kaliru's gates on the same machine.
- **Unit tests against CPython itself**:
  - Vectors are generated by `tools/gen_*_vectors.py` into `tests/data`, covering pyfmt, configparser, shlex and
    cleandoc. 19 tests pass.
  - They found three genuine differences, fixed on main:
    - `repr()` of non-printable Unicode;
    - `int()` / `float()` with Unicode digits, and the float grammar;
    - shlex's escapes in double quotes and its errors.
- **SAFETY notes on every `unsafe` block.**
- **Clippy baseline:** 508 warnings.
  - `result_large_err` 408: `ServerError` is large, which Phase 2f addresses.
  - `let_underscore_future` 51: `send_event` returns a future that is only needed to wait; the work is spawned
    eagerly, and a typed event API (2b) makes that explicit.
  - `type_complexity` 8, `too_many_arguments` 4, then single small ones.
  - `await_holding_refcell_ref` 2 were real latent panics, fixed on main: zeroconf's SSDP loop, and server start.
- **Main fixes from the same review:** the serial reader closed its fd on a read error while the writer still
  used it.

**1. Structure**
- **Leaf modules first:**
  - `pycompat`: Python's own behaviour (pyfmt, configparser, shlex, mimetypes, os.path, urllib quoting, re's
    cache, shutil);
  - `metadata`: the gcode metadata extractor, already a separate `--metadata` mode.

  They depend on nothing of the server's.

  *Measured: modules, not crates.* As workspace crates they cost 130 KB of `.text` (+0.75%) for no runtime gain.
  With `lto = true` and `codegen-units = 1`:
  - each crate monomorphizes its own copy of the shared generics: serde_json, indexmap, hashbrown and regex,
    about 50 KB;
  - LLVM inlines differently across the crate boundary.

  `-Z share-generics`, which would share them, is nightly-only. The symbol diff is `tools/size_diff.sh`.
- **Components in their own modules by concern.** Split the 2k-line files along Moonraker's own seams, for example
  machine into providers, sysinfo, sudo and validation.

*Phase 1 status (2026-10-25)*
- **Leaf modules:**
  - `src/pycompat`: pyfmt, configparser, shlex, inspect, mimetypes, posixpath, pathlib, urllib, re, shutil;
  - `src/metadata`: the extractor and its object cancellation.

  Each Python behaviour is checked by unit tests against CPython's own results.
- **Split by concern**, moved code unchanged:
  - machine, power, simplyprint, application;
  - update_manager (base_deploy and app_deploy, as in Moonraker), git_deploy's GitRepo;
  - mqtt, database, the file observer;
  - confighelper's sources, file_manager's sync lock, authorization's crypto.

  klippy_connection and file_manager are one large impl each, with no seam inside; Phase 3's state split
  divides them.
- **Parity fixes the CPython ports exposed:**
  - `Path::file_name` / `Path::parent` against `os.path.basename` / `dirname`: a trailing `/` or `.`;
  - `Path::extension` / `file_stem` against pathlib's `suffix` / `stem`: a name ending in `.`.

  They also go to main.

**2. Typed core APIs**
- **a. Components.**
  - Typed handles for the built-in components (`server.file_manager()`, ...) instead of a string lookup plus a
    downcast. The string registry stays for Python components and `lookup_component` from Python.
  - An `Any` supertrait with `downcast`, kaliru's convention, instead of per-type `as_any`.
- **b. Events.**
  - One type per event, with a checked payload (kaliru's `Event` trait: `NAME` and typed `Args`).
  - Rust-to-Rust delivery without JSON. Handler order is kept.
  - The name and `Vec<Value>` are made only for the websocket notification and the Python bridge.
- **c. Endpoints.**
  - Typed request arguments: a derive that reads them with the same conversion rules and error texts as
    `WebRequest::get_*`.
  - Typed responses: `#[derive(Serialize)]` structs whose field order is Moonraker's key order, instead of `json!`
    trees.
- **d. Config.** `#[derive(FromConfig)]` structs making the same getter calls in the same order, so access tracking
  (`/server/config`), defaults and error texts stay identical.
- **e. Status and notifications.**
  - Klippy's status stays dynamic: it is Klipper's data, passed through.
  - Moonraker's own state becomes typed structs, serialized once per change:
    - job state;
    - history jobs and totals;
    - file list entries;
    - proc stats;
    - power devices, sensors, spoolman.
- **f. Errors.**
  - `ServerError` stays the boundary type: its message and code are output.
  - Inside, typed errors with context, converted at the edges. `ShellCommandError`, `JwtError` and `RegisterError`
    are the pattern already.

**3. State and ownership**
- Immutable config apart from mutable state: one `RefCell<State>` per component instead of a cell per field.
- Task lifetimes owned by structure: a component's tasks in a `JoinSet` / scope, cancelled with it, instead of
  `JoinHandle`s in cells.
- Fewer clones: borrowing, `Rc<str>` / interned names for keys that repeat (status object names, client ids), `Cow`.

**4. Hot paths** (profiled on the CC1)
- **Status fan-out:** one serialization per update shared by every subscribed client, instead of one per client.
- **JSON-RPC:** borrowed deserialization of incoming requests.
- **File list / metadata / history:** fewer allocations.

Each item ships with a benchmark and a CC1 A/B.

**5. Clean-ups that fall out**
- The plugin layer's dead parts (`is_patched`, `register_class`) are resolved with kaliru. `is_patched` is a parity gap
  (a component's monkeypatch is not honoured by Rust's own calls), tracked on main.

*Progress on 2–5 (2026-10-25)*
- **2a:**
  - `server().get::<T>()` finds a built-in component by type; its name comes from one table (`Named`, in
    components/mod.rs, a test holds it equal to the registry).
  - All 123 string lookups of built-ins use it. `lookup_component(name)` stays for Python components.
- **2b:**
  - The events with Rust handlers are types (src/events.rs): `emit::<E>(args)`, `on::<E>` / `on_async::<E>`.
  - Handlers get the typed arguments (`&Args`, `Rc<Args>`): no JSON, no copy per handler.
  - The values Moonraker passes are made only for websocket notifications and Python handlers, once per send. A
    Python component sending one of these events reaches the Rust handlers (`from_values`).
  - Events without Rust handlers, and those named at run time (`job_state:<state>`), keep `send_event` by name.
  - `send_event` returns `EventSent` and an MQTT publish or serial write returns `Completion`: awaitable
    (`IntoFuture`) and not a `Future`, so fire-and-forget callers need no `let _ =`.
- **2f (part):** `ServerError` boxes its rare parts, AgentError data and a Python traceback. A test holds it at
  48 bytes or less for every `Result`.
- **3a:** a component's background tasks are `TaskSlot` / `TaskSet` (set, replace, cancel, take, cancel_all) instead
  of `JoinHandle`s in `RefCell`s. Behaviour is unchanged: no cancel on drop, `set` detaches as overwriting a
  handle did.
- **4 (part):**
  - A notification is encoded once, from borrowed values, and shared by every client's queue (`Utf8Bytes`,
    reference counted), instead of two copies of the values plus one string per client.
  - Status updates stay per client: each client has its own subscription.
- **5:** `is_patched` is documented as a known difference on main, and both dead parts are allowed as dead code,
  agreed with kaliru.
- **Clippy:** 508 warnings down to the plugin layer's 11, which is shared with kaliru.
  - Keyword arguments became option structs with Moonraker's defaults (`RunOpts`, `DownloadOpts`, beside
    `RequestOpts` and `ExecOpts`).
  - Complex tuples got named aliases.
  - `rust-version = "1.89"`: what the dependencies already require.

*Where the plan meets parity (decided 2026-10-25, with the evidence)*
- **2e, typed state: only where Moonraker fixes the types.** Much of Moonraker's state passes Klipper's or
  Python's dynamic types through to the output, and serde would change them:
  - history's `PrinterJob` copies print_stats as Klipper sends it, so `filament_used` is the int `0` until Klipper
    reports `0.0`;
  - `get_path_info` returns `{"modified": 0, ...}`, an int, when `stat()` fails, and `st_mtime`, a float,
    otherwise;
  - file list entries merge their metadata dicts.

  A typed `f64` cannot be both, so these stay `serde_json::Value`, which is what they are in Moonraker.
- **2c, typed request arguments: in place already.**
  - `WebRequest::get_int` / `get_float` / `get_str` / `get_boolean` (with `_or` / `_opt`) are typed accessors.
  - Each uses Moonraker's conversion rules and error texts, called in Moonraker's order, which decides which
    error a bad request gets.
  - A derive would restate the same calls.
- **2c, typed responses: `json!` stays.** The responses are mostly Moonraker's dynamic dicts (above).
- **2d, config structs: in place in substance.**
  - Each component reads its options in `new()`, in Moonraker's order (access tracking and `/server/config`
    depend on it), into typed fields.
  - A derive would restate the getters.
- **3b, one `RefCell<State>` per component: not done.**
  - A single cell widens every borrow to the whole state.
  - This code base had two borrows held across an await (zeroconf SSDP, server start), which a wider cell makes
    more likely.
  - Fine-grained cells keep each borrow to its field.
- **3c, `Rc<str>` / interning, and 4's other hot paths: by profile only.** Under the resource gate's load (20
  status updates/s, 2 clients), Moonruster uses about 3.6 ms of CPU per second. A change needs a measured gain:
  `perf` locally, and a CC1 A/B when the printer is free.

Out of scope:
- behaviour changes;
- COSMOS (branch `cosmos`) and Rusticc (patches in Rusti3D/rusticc), rebased after rustify milestones.
