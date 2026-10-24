# Python components in Moonruster: plan (2026-10-24, draft)

Requirement (user, 2026-10-24): **if a component works in Moonraker, it works in Moonruster**, the same way
kaliru runs any plugin that works in Kalico. Moonruster stays a self-contained drop-in replacement for Moonraker,
and kaliru stays one for Kalico. Neither depends on the other at run time. The approach and tech stack are
kaliru's (`kaliru/docs/PLUGINS.md`, branch `plugins`): the host's CPython, loaded with dlopen only when a
component needs it, plus a generated mirror of the original's Python surface over the Rust objects.
Memory rule, as in kaliru:
- no cost when no Python component is configured;
- no more than Python Moonraker costs when one is.

## What Moonraker offers a component (the contract to match)

Source: Moonraker 9e676eba (`ref/moonraker`), `moonraker/server.py`, `moonraker/utils/__init__.py`,
`docs/components.md` and `docs/installation.md`.

1. **Where components come from.** A third-party component is a module in the Moonraker source tree's
   `moonraker/components/` package, usually symlinked there by its installer:
   - Happy Hare symlinks into `$(MOONRAKER_HOME)/moonraker/components` (`Makefile:488`), default `~/moonraker`
     (`installer/Kconfig.paths_services`);
   - Moonraker's docs name Moonraker-Timelapse as the example and say the pip-package install is for users
     who do not run such extensions (`docs/installation.md` "Installing the Moonraker Python package").
2. **Loading** (`server.py` `load_components` / `load_component`, lines 267-333):
   - `database` loads first, then `CORE_COMPONENTS` (line 61);
   - then every other config section, in Python `set` order (`cfg_sections = set(...)`), with
     `importlib.import_module("moonraker.components.<name>")`;
   - `load_component(config)` is called with `config.getsection(name)` (or `[server]` as fallback for core
     components); the result goes into `server.components`;
   - failures are logged with the traceback and added to `failed_components`;
   - a component may load another while loading (`server.load_component(config, "spoolman", None)`), but not
     after configuration ends ("Cannot load components after configuration").
3. **Pip recovery.** If the import fails with `ModuleNotFoundError` for another module, and the component is not
   in `unofficial_components` (untracked `components/*.py` in the git checkout, `utils/__init__.py:92-140`),
   Moonraker runs `pip install -r scripts/moonraker-requirements.txt` once and retries (`try_pip_recovery`).
4. **Lifecycle.** Moonraker calls `component_init()` (sync or async) after loading (`server.py:185, 258`),
   `on_exit()` at shutdown (`:456`) and `close()` (`:490`), if the component has them.
5. **Everything else is unrestricted**, as with Kalico plugins. A component gets the real Python objects and can:
   - call any method: `lookup_component`, `register_endpoint`, `register_remote_method`,
     `register_event_handler`, `register_notification`, `send_event`, `get_host_info`, `klippy_apis.*`,
     `database.*`, the `EventLoop` wrapper and `eventloop.asyncio_loop`;
   - read and write private attributes, wrap other components' objects, monkeypatch methods, and subclass
     Moonraker classes;
   - import any module of the `moonraker` package (69 modules, 222 classes, 121 module functions and 2164
     methods, 633 of them `async`; counted by AST, `thirdparty/` excluded);
   - import Moonraker's dependencies directly (`scripts/moonraker-requirements.txt`: tornado, pillow, jinja2,
     paho-mqtt, zeroconf, dbus-fast, ...), from Moonraker's virtualenv.
6. **asyncio is part of the contract.** Moonraker runs on a real asyncio loop, and components depend on it:
   - Moonraker methods return real `asyncio.Future`s (timelapse checks `isinstance(x, asyncio.Future)`);
   - components use `asyncio` primitives (Lock, Event, sleep, create_task, wait_for);
   - timelapse calls `tornado.ioloop.IOLoop.current()`, which only works with a running asyncio loop.

## Corpus and Phase 0 trace (2026-10-24)

Corpus components, as git clones in `Originals_Base/addons/corpus/`, each symlinked into `moonraker/components`
the way its installer does:

| component | repo @ commit | lines | under Moonraker 9e676eba |
|---|---|---|---|
| timelapse | mainsail-crew/moonraker-timelapse @ c7fff11 | 841 | loads |
| mmu_server | Happy-Hare @ 70aa90f (already in the corpus) | 2387 | loads |
| uboe_metadata, additional_pre_print_checks, spool_change_estimator | CooperGerman/uboe_moonraker_plugins @ 6bbfb8a | 339 + 934 + 224 | load |
| helix_print | prestonbrown/helixscreen @ 00a9fe6 (`moonraker-plugin/`) | 2083 | loads; its `helix_storage` sibling fails to import (its own bug, same on Moonraker) |
| filaman | Fire-Devils/FilaMan-Moonraker-Komponente @ 669a0ca | 1406 | loads only without `[spoolman]` (both register `/server/spoolman/*`) |
| geeks3d | 2BWorks/Moonraker-3DGeeks-Component @ efc730a (2021) | 335 | fails: json-dumps a `Future` from the database. It must fail the same way |

How the trace was made:
- **Static:** `tools/plugin_trace/static_scan.py` (AST: imports, handles from `lookup_component`/`load_component`,
  attributes used, writes, subclasses).
- **Dynamic:** `tools/plugin_trace/tracer.py` runs Python Moonraker with every Moonraker class and module
  instrumented, recording each attribute a component reads or writes and what Moonraker reads back from
  component objects.
  - `setup_trace.sh` builds the tree and instance (`~/mr/trace`, `~/mr/inst/trace`, port 7030); `run_trace.sh`
    starts it.
  - `exercise.py` drives every registered endpoint and remote method (through a Klipper macro each), uploads,
    prints with pause, resume and cancel, a Klipper restart, and a clean shutdown.
  - Two variants: the default (`[spoolman]`) and `filaman`.
- The merged result is `tools/plugin_trace/surface.md`.

What the trace established beyond points 1-6 above (each one checked in the trace or log, not inferred):
- **Components import each other** as `moonraker.components.<name>`, for example
  `from ..components.uboe_metadata import ExtrusionPoints`. Third-party files must be real submodules of that
  package.
- **Components register under built-in names.** FilaMan calls `server.register_component("spoolman", self)` to
  stand in for the built-in. Moonraker code that later looks up `spoolman` gets the Python object, and
  `/server/info` lists it.
- **Load order is random in Moonraker.** Optional sections load in Python `set` order. With string hash
  randomisation, that order changes on every run (checked: four runs, four orders).
  - This decides which of `[spoolman]` and `[filaman]` loads.
  - It also decides whether `spool_change_estimator` finds `additional_pre_print_checks`: "Component not found"
    in one run, loaded in the next.
  - Moonruster may use any order Moonraker could produce. Use config order, which makes it deterministic.
- **Partial registration persists.** geeks3d registers 5 endpoints in `__init__`, then fails. Moonraker keeps
  serving them (HTTP 200) while listing geeks3d in `failed_components`.
- **Module globals change Moonraker's behaviour.**
  - Happy Hare sets `file_manager.file_manager.METADATA_SCRIPT` to its own file (traced as a write).
  - Moonraker then runs metadata extraction as `sys.executable <METADATA_SCRIPT> -c <cfg>`
    (`file_manager.py:2613`; log: `/root/mr/venv/bin/python …/components/mmu_server.py -m -n -c
    /tmp/metacfg-….json`).
  - That script imports Moonraker's own `file_manager/metadata.py` from disk.
  - So when the global is patched, Moonruster must run that subprocess instead of its native extractor.
- **`sys.executable` is used.** uboe_metadata runs itself as a script with `sys.executable` (`shell_command`
  `exec_cmd`), so the embedded interpreter's `sys.executable` must be the virtualenv `python`.
- **Class identity is used.** helix_print looks up `PrinterJob` through
  `sys.modules[type(self.history).__module__]`, so mirrored classes must report Moonraker's `__module__` and
  `__qualname__`.
- **Direct tornado and pip imports.** timelapse uses `tornado.ioloop.IOLoop.current()`; geeks3d uses
  `AsyncHTTPClient`, `IOStream` and `gen`; uboe uses `packaging`.
- **What Moonraker reads back from component objects:** only `component_init`, `on_exit` and `close`. Every
  other Rust-to-Python call goes through callables the components passed in:
  - endpoint handlers (given a `WebRequest`);
  - event handlers (Moonraker's own payload types, such as `JobEvent` and dicts);
  - remote methods (keyword arguments from Klipper);
  - `FlexTimer` and `register_callback` callbacks;
  - announcement feeds and history auxiliary fields.
- **Private members used:**
  - dynamic: `SpoolManager.database` and `SpoolManager.spoolman_url` (mmu_server);
  - static only: `klippy_apis._send_klippy_request`, `spoolman._get_response_error`, and
    `mmu_server._init_mmu_backend` (called by uboe).
- **Installers** look for `~/moonraker` first: Happy Hare (`installer/Kconfig.paths_services`), helix
  (`install.sh` `find_moonraker_components`) and timelapse (its docs).
  - helix then probes the running process for `moonraker.py`, which finds nothing under Moonruster.
  - So decision 1's default matches what installers do.

## Design (kaliru's approach)

1. **Interpreter** (proven in Phase 1). The host's CPython, loaded with `dlopen` the first time a config section
   names a component that Moonruster does not have built in. It is the Python Moonraker ran under: the
   virtualenv's `python` (`~/moonraker-env`), or the system `python3` where Moonraker had no virtualenv (COSMOS).
   - **Finding libpython.** Moonruster asks that `python` once for `sysconfig` `LIBDIR`/`INSTSONAME` and its
     identity; for relocated builds it uses `<base_prefix>/lib`.
   - **Starting it.** `Py_SetProgramName(<that python>)`, then `Py_InitializeEx(0)`. CPython then finds
     `pyvenv.cfg` itself, as the real executable does. Executable, prefixes, `sys.path`, `sys.flags` and
     version match `python -c` exactly on 3.10, 3.11, 3.12, 3.13 and 3.15. The only module difference is
     `_signal`, which `initsigs=1` imports.
   - **Future fallback.** If a later CPython removes `Py_SetProgramName`, PEP 741's `PyInitConfig` (3.14+) is
     the replacement. It starts from the isolated configuration, so `isolated=0`, `use_environment=1`,
     `user_site_directory=1` and `safe_path=0` are set; that path also matches exactly (checked on 3.15).
   - **The C-API table** is copied from kaliru (`capi.rs`) and cut down to functions every CPython from 3.10
     on exports (Moonraker's `requires-python`; checked with `nm -D` on each version).
   - **Linking.** Moonruster must be a dynamically linked glibc executable: a static musl binary cannot load
     libpython at all ("Dynamic loading not supported", tested). See "Build" below.
2. **Discovery.** The loader no longer stops at the fixed `COMPONENT_NAMES`:
   - a name Moonruster has built in loads the Rust component, as now;
   - otherwise Moonruster looks for `<name>.py` or `<name>/__init__.py` in the components directory of the
     Moonraker tree it replaced (see open decision 1), then imports it as `moonraker.components.<name>`;
   - load order, failure handling, log text, `failed_components`, pip recovery and the lifecycle calls follow
     `server.py` exactly.
3. **Module namespace.** A meta-path finder serves `moonraker`, `moonraker.common`,
   `moonraker.components.<built-in>` and the rest from the generated mirror. Third-party component files and the
   site-packages are imported normally.
4. **Mirror.** The Moonraker equivalent of kaliru's `tools/gen_mirror.py` (`tools/gen_mirror.py` here):
   - **every name:** every module, class, function, method and attribute of Moonraker 9e676eba, with the same
     signatures; methods call into Moonruster through the native module, and an AST diff must come back empty;
   - **original code where it fits:** exception classes and pure-Python helpers without Rust state are
     Moonraker's own code, as kaliru does for some Kalico code;
   - **live attributes:** reads see current Rust state, and writes change it;
   - **one wrapper per Rust object**, so `is` holds;
   - **overrides:** Object/Meta/Attr as in kaliru's prelude. Instance or class patches and Python subclasses
     of mirrored classes are reported to Moonruster, which calls the Python replacement from its own call
     sites too.
5. **asyncio on Moonruster's loop** (proven in Phase 1, `pybridge/src/aio.py`, `lib.rs`). Moonruster is
   single-threaded (tokio `current_thread` + `LocalSet`, `main.rs:202`), so Python coroutines run on the same
   thread, and kaliru's greenlet thread-state swapping is not needed.
   - **The loop.** CPython's own `SelectorEventLoop`, Moonraker's default: its installer leaves uvloop out
     (`SPEEDUPS=n`), and `MOONRAKER_ENABLE_UVLOOP=n` is documented. COSMOS installs uvloop; it has the same API but
     no step function, so it cannot be driven from outside.
   - **Only the selector differs:** its `select()` never blocks.
   - **Setup:** the loop is set up exactly as `run_forever()` sets it up. 3.13+ call `_run_forever_setup()`,
     which exists for loops integrated with another event loop (its docstring). 3.10-3.12 repeat the same
     inline steps (checked in each version's `base_events.py`).
   - **Waiting:** tokio waits on the selector's epoll fd (`AsyncFd`), the next asyncio timer and a `Notify` for
     wakeups from Rust, then runs one `_run_once()`. Readiness is cleared only after a zero-timeout `poll`
     shows the fd idle; tokio's `clear_ready` only clears readiness older than the guard (`async_fd.rs:997`).
   - **Futures both ways:** Rust async calls return real `asyncio.Future`s, resolved by a tokio task. Python
     coroutines that Rust awaits complete a oneshot through `add_done_callback`.
6. **GIL and threads** (proven). The loop thread holds the GIL except while tokio parks, through tokio's
   `on_thread_park`/`on_thread_unpark`. In tokio 1.54.0, `current_thread/mod.rs:411-438` calls them around
   every park, blocking or zero-timeout. Python threads (executors, `call_soon_threadsafe`, component threads)
   run at full speed while the loop idles and not at all while Rust holds the GIL, as under Moonraker, where
   Moonraker's own Python holds it.
7. **Errors.** Python `ServerError` and the other `common` exceptions map both ways to Moonruster's
   `ServerError` (`src/common.rs`). Responses and logged tracebacks match Moonraker's format.

8. **Pitfalls kaliru hit** (from the kaliru session, 2026-10-24; already fixed there):
   - the layer's tables are GIL-guarded process statics, not `thread_local!`, because Python threads call in;
   - `raise` sets a fallback `RuntimeError` when building the exception fails, and error conversion has a
     re-entry guard;
   - the prelude module gets `__builtins__` before `PyEval_EvalCode`;
   - overrides are dispatched in the callee, with a bypass stack, so a patch that calls the original method does
     not recurse;
   - the generator's free-name check counts only imports it actually emits;
   - parity runs need the same CPython on both sides, because logged tracebacks contain stdlib paths;
   - a CPython debug build (`libpython3.15d`) catches C-API misuse where it happens.

## Phase 1 results (2026-10-24, `pybridge/`)

`pybridge/src/bin/proto.rs` runs CPython on a tokio `current_thread` runtime + `LocalSet`, as Moonruster runs.
Each check has a control that must fail:

| check | result | control (must fail) |
|---|---|---|
| identity vs `python -c` | equal on 3.10, 3.11, 3.12, 3.13, 3.15 and on the CC1's COSMOS 3.15.0 | - |
| asyncio sleep accuracy, `gather`, `wait_for` timeout, cancel, running loop | pass | `nodriver`: all fail, then hang |
| tokio not blocked while Python waits | 40-41 ticks of a 5 ms ticker in 0.2 s | `blocking` (normal selector): hangs at the first socket test |
| tornado `IOLoop.current()`, tornado HTTP server + `AsyncHTTPClient`, asyncio sockets | pass | |
| Rust future awaited from Python (result and exception) | pass | |
| Python coroutine awaited from Rust | pass (every check runs this way) | |
| executor, `call_soon_threadsafe` from a thread | pass, wakeup 0.2-1.2 ms | `nohooks`: fail |
| Python thread vs GIL | 93-105 % of full speed while the loop idles; exactly 0 while Rust holds the GIL | `nohooks`: 35 % |
| asyncio subprocess alongside a tokio child process | both exit codes right | |
| asyncio `add_signal_handler`, then a tokio signal | both delivered | |

Costs. Own memory is `Pss_Anon + SwapPss`; on the CC1, zram swap hides pages from `Pss_Anon` alone:

| | x86_64, CPython 3.12 | CC1 (armv7, COSMOS CPython 3.15.0) |
|---|---|---|
| before Python is loaded | libpython not mapped | libpython not mapped, 100 kB |
| ask the venv's python about itself | 25-72 ms | 410-472 ms |
| interpreter start | 11-97 ms | 149 ms |
| own memory after start | +3.1-4.1 MB | +2.1 MB |
| + the asyncio loop | 10.4 MB total | 7.6 MB total |
| after every check (tornado HTTP included) | 16.9 MB total | 12.5 MB total |
| Python -> Rust call | 0.03-0.08 us | 0.92-0.95 us (a Python -> Python call: 0.83) |
| Rust -> Python call | 0.03-0.06 us | 0.60 us |

## Build: glibc, non-PIE, tuned malloc

Python components need `dlopen`, so Moonruster becomes a dynamically linked glibc executable instead of a
static musl one (Kaliru already is). Measured on current Moonruster (no component code) with `tools/bench.sh`,
idle and with 20 status updates/s to 2 clients:

| build | x86_64 idle / load | CC1 idle / load | CC1 CPU under load |
|---|---|---|---|
| static musl (as shipped) | 1.11 / 1.15 MB | 816-880 / 896-904 kB | 85-87 ticks/30 s |
| glibc, PIE, default malloc | 2.19-2.32 / 2.28-2.37 MB | - | - |
| glibc, non-PIE, default malloc | 1.89 / 1.83-1.87 MB | 1088-1224 / 1420-1488 kB | 77-78 |
| glibc, non-PIE, `top_pad=0`, `trim_threshold=0` | 1.34 / 1.42 MB | 944-992 / 1140-1144 kB | 76-78 |

Where glibc's extra memory came from (per-mapping `Anonymous`, x86):
- **PIE relocations:** 492 kB of the executable's read-only data was patched at load and turned into private
  memory. Linking non-PIE (`-C relocation-model=static`) removes it; `dlopen` of libpython does not need a PIE
  executable.
- **malloc padding and retention:** about 800 kB more heap than musl for the same allocations. Setting
  `M_TOP_PAD` and `M_TRIM_THRESHOLD` to 0 removes most of it. Moonruster sets them with `mallopt()` at start, so
  init scripts need no environment variable. `arena_max=1` made no difference.

Net on the CC1: **+100 kB idle, +240 kB under load, about 10 % less CPU** than musl.

The release glibc floor still has to be chosen: the cross toolchain here has glibc 2.39, the CC1 2.43,
Debian bookworm 2.36 and bullseye 2.31. zig can target an older glibc.

## Phase 2 results (2026-10-24, mirror)

`tools/gen_mirror.py ref/moonraker OUT` generates 72 modules and 3188 IDs; 76 classes are Moonraker's own
code. `mirror.bin` is 131 kB compressed. Changes from kaliru's generator, each one forced by something in
Moonraker's source:
- **Coroutines and generators.** Async methods `return await _mr.call(...)`, so they stay coroutine
  functions. The 5 generator and async-generator methods keep iterator semantics.
- **Annotations are kept**, as strings (PEP 563), so `inspect.signature` reads as Moonraker's does.
- **Classes kept as Moonraker's own code (verbatim):** exceptions, enums and flags, dataclasses, metaclasses,
  and classes on stdlib or third-party bases, transitively across modules. Their runtime globals are bound at
  import, because a function's globals are a plain dict lookup that never reaches the module's `__getattr__`.
  Dataclasses also get `ClassVar`/`InitVar`.
- **Whole modules kept verbatim:** `utils.json_wrapper`, which keeps its msgspec switch,
  `utils.ioctl_macros` (the V4L2 constants are computed with it at import) and
  `components.file_manager.metadata` (Moonraker's standalone metadata script).
- **Anything that needs a third-party package is lazy:** a statement, class or class attribute that needs one
  is Moonraker's own statement, run on first access.
  - Examples: `WATCH_FLAGS`, paho's tables, mqtt's `ExtPahoClient`, tornado's handler classes,
    `DbusManager.Variant`.
  - Every imported name exists too, so re-exports like `from ..utils import ServerError` resolve.
  - A missing package is an `AttributeError`, as in Moonraker, where the module would simply not have the
    name.
- **Side effects on third-party packages** run at startup for modules Moonraker imports at startup:
  `http_client`'s `AsyncHTTPClient.configure(user_agent='Moonraker', max_body_size=...)`.
  - One documented exception: `eventloop`'s uvloop policy switch is not run, because Moonruster drives the
    stdlib loop.
- **ABC classes** (`common.RenderableTemplate`) get `_mr.AbcMeta`, a `Meta` + `ABCMeta`. Other classes keep the
  plain `Meta`; ABCMeta on all of them would add `_abc_impl` everywhere.
- **Module paths.** Mirror modules carry the real Moonraker tree's file paths as `__file__`, so values derived
  from it, such as `METADATA_SCRIPT`, are Moonraker's.

Check: `tools/mirror_surface.py` dumps the surface of real Moonraker and of the mirror and diffs them. It
covers every module's names, signatures, async and generator kinds, decorators, bases, enum members and plain
values.
- **With Moonraker's venv**, with every lazy name evaluated: **0 differences** against Moonraker 9e676eba on
  CPython 3.12.
- **Bare**, without site-packages: the mirror loads without importing any third-party package. The only
  differences come from absent packages: the lazy names, msgspec, and `metadata`, which, like Moonraker's own
  `metadata.py`, imports PIL.
- **Known structural differences:** a mirrored class's root base is `_mr.Object`, not `object`. Classes on a
  third-party base are lazy, so they are Moonraker's own classes.

Memory, x86 CPython 3.12, loading **all** 72 mirror modules (a component imports a handful):
- the stdlib modules they import, which Moonraker imports too, chiefly asyncio and logging: 9.8 MB;
- the prelude plus every mirror module: 4.7-5.2 MB.

The prelude does not import `importlib.abc`, which alone costs 4.1 MB.

## Sharing with kaliru

- Code is copied, not linked: each copied file names its kaliru source file and commit. Moonruster stays
  self-contained, and the two sessions do not edit each other's files.
- Memory: the libpython pages are file-backed and shared between the two processes. Each process has its own
  interpreter heap (kaliru measured +1.7 MB anon on the CC1). A shared interpreter was rejected:
  - a Klipper `RESTART` would take Moonruster's components down;
  - Moonruster would depend on kaliru;
  - a crash in one could stop the other.

## Phases (each ends with a gate)

0. **Scope.** Done: this document and the trace above (`tools/plugin_trace/`). More real components join the
   corpus as they are found.
   - Gate: user agrees on the open decisions below.
1. **Prototype. Done**, see "Phase 1 results" (standalone `pybridge/` crate, as in kaliru), each check with a
   control that must fail:
   - CPython's asyncio loop driven from a tokio `LocalSet`;
   - `asyncio.sleep`, `create_task`, `wait_for` with timeout, cancellation, `IOLoop.current()`;
   - a Rust future awaited from Python, and a Python coroutine awaited from Rust;
   - a Python thread blocked on the GIL while Rust holds it, and running while tokio parks;
   - `run_in_executor` calling back into the loop;
   - measure memory, crossing cost and startup on x86, then armv7 on real hardware only with the user's OK.
   - Gate: all checks pass and costs are within the memory rule.
2. **Mirror tooling. Done**, see "Phase 2 results".
   - `tools/gen_mirror.py` for `ref/moonraker` generates the mirror modules and the ID table.
   - `tools/mirror_surface.py` reports nothing missing or different.
   - Every module imports.
3. **Loader parity.**
   - Discovery and third-party modules as `moonraker.components.<name>` submodules.
   - Load order: config order, as decided above.
   - Failure handling and logs, with partial registrations kept.
   - Pip recovery and the lifecycle calls.
   - `register_component` under built-in names, with Rust lookups of those names reaching the Python object.
   - `sys.executable` is the virtualenv `python`.
   - `/server/info` (`components`, `failed_components`, `warnings`). Each gets cases against Python Moonraker in the
   existing harnesses (`tools/difftest.py`, `ext_test.py`, `scenario.py`) with test components on both sides.
4. **Handlers, driven by the corpus.**
   - Rust handlers for the mirrored methods: server, the component wrappers, `WebRequest`, `klippy_apis`,
     `database`, `file_manager`, `http_client`, `spoolman`, `webcam`, `shell_command` and `eventloop`.
   - A parity case per area, plus override cases (a test component that patches or subclasses built-ins).
   - Module globals that steer Rust behaviour, first `file_manager.METADATA_SCRIPT`: when it is patched,
     metadata runs as Moonraker's subprocess (`sys.executable <script> -c <cfg>`).
   - Event payloads in Moonraker's Python types (`JobEvent` and the rest), checked by a test component that
     records what its handlers receive on both servers.
5. **Corpus end to end.** Timelapse and Happy Hare's `mmu_server` on both servers, with the same config, a mock
   or real Klipper, and identical API responses, notifications and database contents. Real hardware only with
   the user's OK.
6. **No regression and costs.**
   - The existing suites are unchanged without components.
   - Binary size and idle CPU are measured before and after.
   - A cargo feature can compile the layer out.

## Decisions (user, 2026-10-24: "full parity with Moonraker", "keep memory in check")

1. **Components directory.** Default `~/moonraker/moonraker/components`, overridable by an env var or
   argument. That is the tree Happy Hare's, helix's and timelapse's installers write into, and the one
   INSTALL 2a leaves in place. On a new install without Moonraker, an empty `moonraker/components` next to the
   binary, which installers can be pointed at.
2. **Interpreter.** The `moonraker-env` virtualenv's `python` when present, else the system `python3`.
   CPython >= 3.10, Moonraker's own minimum (Phase 1: 3.10-3.15 verified).
3. **Pip recovery.** As Moonraker: one `pip install -r scripts/moonraker-requirements.txt` in that Python.
4. **Reference version.** The mirror is generated from `ref/moonraker` 9e676eba. Upstream's newest commit,
   b559b9a, is one commit later (`klippy_connection` only) and loads components the same way. If
   `ref/moonraker` moves, the mirror is regenerated and the surface check rerun.
5. **Build.** Release binaries become glibc, non-PIE, with malloc tuned at start ("Build" above).

## What "works in Moonraker" cannot cover

- **Timing.** Moonruster answers faster.
- **Files on disk.** A component that reads Moonraker's own source files or git state needs the Moonraker tree
  present (it is, for replace installs).
- **Python version and packages.** These are the host's, as under Moonraker.
