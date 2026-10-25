#!/usr/bin/env python3
# Python component API coverage (docs/PLUGINS.md): for each class a
# component gets from lookup_component() (mirror_ids.rs COMPONENT_CLASSES),
# its public methods and attributes and whether Moonruster serves them: a
# Rust handler (src/plugins/impls), Moonraker's own code (mirror_verbatim
# .txt) or lifecycle the server itself calls.
# usage: python tools/api_coverage.py [--missing]
import ast
import glob
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
REF = ROOT / "ref/moonraker"
txt = (ROOT / "src/plugins/mirror_ids.rs").read_text(encoding="utf-8")
ids = re.findall(r'"([^"]+)"', txt.split("pub static IDS")[1].split("];")[0])
classes = re.findall(r'\("(\w+)", "([\w.]+)", "(\w+)"\)', txt.split("COMPONENT_CLASSES")[1])
# what every component also gets: its config and the server
classes += [("confighelper", "moonraker.confighelper", "ConfigHelper"), ("server", "moonraker.server", "Server"),
            ("eventloop", "moonraker.eventloop", "EventLoop")]
src = "".join(open(f, encoding="utf-8").read() for f in glob.glob(str(ROOT / "src/plugins/impls/*.rs")))
handled = set(re.findall(r'"(moonraker\.[A-Za-z0-9_.]+)"', src))
for mac, prefix in re.findall(r'macro_rules! (\w+) \{\s*\(\$m:literal\) => \{\s*concat!\("([^"]+)"', src):
    handled |= {prefix + m for m in re.findall(mac + r'!\("([A-Za-z0-9_.]+)"\)', src)}
# names built in loops or field macros (prefix + literal list)
for prefix, body in re.findall(r'concat!\("([^"]+)", \$\w+, "([^"]+)"\)', src):
    pass
verbatim = {l.split("#")[0].strip() for l in open(ROOT / "tools/mirror_verbatim.txt") if l.split("#")[0].strip()}
LIFECYCLE = {"component_init", "close", "on_exit", "__init__"}
# what the server or the component itself calls, not an API for other
# components (docs/PLUGINS.md "Not provided"), by class: method -> reason
INTERNAL = {
    "application": {"listen": "server start", "register_websocket_handler": "takes a tornado handler class"},
    "authorization": {"authenticate_request": "takes a tornado request"},
    "extensions": {"start_unix_server": "server start", "on_unix_socket_connected": "server's socket callback"},
    "file_manager": {"start_file_observer": "server start"},
    "history": {m: "the history's own job tracking" for m in
                ("add_job", "save_job", "finish_job", "grab_job_metadata", "update_metadata", "send_history_event")},
    "klippy_connection": {"load_saved_state": "server start", "connect": "server start",
                          "open_klippy_connection": "server start",
                          "register_method_from_agent": "an agent connection's own registration"},
    "notifier": {"register_remote_actions": "called once by its __init__", "register_endpoints": "called once by its __init__"},
    "paneldue": {m: "PanelDue's own serial loop" for m in
                 ("run_serial", "paneldue_beep", "process_line", "queue_gcode", "queue_command",
                  "handle_gcode_response", "write_response")},
    "simplyprint": {m: "SimplyPrint's own connection" for m in ("save_item", "send_status", "send_sp")},
    "websockets": {m: "a connection's own registration" for m in
                   ("add_client", "remove_client", "add_bridge_connection", "remove_bridge_connection")},
    "machine": {"get_system_provider": "the provider is Moonruster's (service control: do_service_action)"},
    "confighelper": {"get_source": "the config source is Moonruster's"},
    "server": {m: "server start" for m in ("server_init", "start_server", "run_until_exit", "load_components")},
}


def methods_of(module, cls):
    """public method names (and whether async) of Moonraker's class"""
    parts = module.split(".")[1:]
    path = REF / "moonraker" / pathlib.Path(*parts)
    path = path / "__init__.py" if path.is_dir() else path.with_suffix(".py")
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == cls:
            return {n.name for n in node.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))}
    return set()


def ok(q):
    return q in handled or q in verbatim or any(q.startswith(v + ".") for v in verbatim)


missing_only = "--missing" in sys.argv
tot_m = tot_mh = tot_a = tot_ah = 0
for comp, module, cls in classes:
    meths = methods_of(module, cls)
    pub = [n for n in ids if n.startswith(f"{module}.{cls}.") and not n.split(".")[-1].startswith("_")]
    m = [n for n in pub if n.split(".")[-1] in meths and n.split(".")[-1] not in LIFECYCLE and not n.endswith(".setter")]
    a = [n for n in pub if n.split(".")[-1] not in meths]
    mh, ah = [n for n in m if ok(n)], [n for n in a if ok(n)]
    internal = INTERNAL.get(comp, {})
    m = [n for n in m if n.split(".")[-1] not in internal]
    mh = [n for n in m if ok(n)]
    miss = [n.split(".")[-1] for n in m if not ok(n)]
    tot_m += len(m); tot_mh += len(mh); tot_a += len(a); tot_ah += len(ah)
    if missing_only:
        if miss:
            print(f"{comp}: {' '.join(miss)}")
    else:
        print(f"{comp:22} methods {len(mh):3}/{len(m):<3} attrs {len(ah):3}/{len(a)}")
print(f"TOTAL methods {tot_mh}/{tot_m}, attributes {tot_ah}/{tot_a}")
