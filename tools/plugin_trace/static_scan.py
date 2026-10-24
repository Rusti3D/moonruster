#!/usr/bin/env python3
# Static surface scan of third-party Moonraker components (docs/PLUGINS.md
# Phase 0): for each component file, which parts of Moonraker it reaches.
#  - imports of moonraker modules (runtime vs TYPE_CHECKING only)
#  - components looked up / loaded by name, and the attributes used on them
#    (handles tracked through `x = server.lookup_component("name")`,
#    `self.x = ...` and `config.get_server()`)
#  - attributes used on the server, config and imported moonraker modules
#  - writes to moonraker objects or modules (patches), subclasses of
#    moonraker classes
# usage: static_scan.py COMPONENT.py... (prints JSON)
import ast
import json
import sys
from collections import defaultdict


def is_type_checking(node):
    t = node.test
    return (isinstance(t, ast.Name) and t.id == "TYPE_CHECKING") or (
        isinstance(t, ast.Attribute) and t.attr == "TYPE_CHECKING")


def moonraker_module(mod, level):
    """Absolute moonraker module for an import inside moonraker/components"""
    if level == 0:
        return mod if mod and mod.split(".")[0] == "moonraker" else None
    base = ["moonraker", "components"][: 2 - (level - 1)]
    if level > 2:
        return None
    return ".".join(base + ([mod] if mod else []))


def handle_key(node):
    """'self.x' or 'x' for a handle-holding expression, else None"""
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id == "self":
        return "self." + node.attr
    return None


def scan(path):
    tree = ast.parse(open(path, encoding="utf-8").read(), path)
    out = {
        "imports": defaultdict(set), "imports_typing": defaultdict(set),
        "components": defaultdict(set), "server": set(), "config": set(),
        "modules": defaultdict(set), "writes": set(), "subclasses": set(),
    }
    # pass 1: imports, and the names they bind
    bound = {}  # local name -> moonraker qualified name
    typing_nodes = set()
    for n in ast.walk(tree):
        if isinstance(n, ast.If) and is_type_checking(n):
            for s in ast.walk(n):
                typing_nodes.add(id(s))
    for n in ast.walk(tree):
        if isinstance(n, ast.ImportFrom):
            base = moonraker_module(n.module, n.level)
            if base is None:
                continue
            dest = out["imports_typing"] if id(n) in typing_nodes else out["imports"]
            for a in n.names:
                dest[base].add(a.name)
                if id(n) not in typing_nodes:
                    bound[a.asname or a.name] = base + "." + a.name
        elif isinstance(n, ast.Import):
            for a in n.names:
                if a.name.split(".")[0] == "moonraker":
                    dest = out["imports_typing"] if id(n) in typing_nodes else out["imports"]
                    dest[a.name].add("*")
                    bound[a.asname or a.name] = a.name
    # pass 2: handles (server, config, components); repeated until stable
    handles = {}  # key -> "server" | "config" | "component:<name>"
    for func in [n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))]:
        for a in func.args.args:
            ann = ast.unparse(a.annotation) if a.annotation is not None else ""
            if a.arg == "config" or ann.strip("'\"").endswith("ConfigHelper"):
                handles.setdefault(a.arg, "config")
    for _ in range(4):
        for n in ast.walk(tree):
            if isinstance(n, (ast.Assign, ast.AnnAssign)):
                targets = n.targets if isinstance(n, ast.Assign) else [n.target]
                kind = classify(n.value, handles) if n.value is not None else None
                if kind:
                    for t in targets:
                        k = handle_key(t)
                        if k:
                            handles[k] = kind
    # pass 3: uses
    for n in ast.walk(tree):
        if isinstance(n, ast.Attribute):
            k = handle_key(n.value)
            kind = handles.get(k) if k else None
            if kind == "server":
                out["server"].add(n.attr)
            elif kind == "config":
                out["config"].add(n.attr)
            elif kind and kind.startswith("component:"):
                out["components"][kind[10:]].add(n.attr)
            elif isinstance(n.value, ast.Name) and n.value.id in bound:
                out["modules"][bound[n.value.id]].add(n.attr)
            # chained lookups: server.lookup_component("x").attr
            inner = classify(n.value, handles)
            if inner and inner.startswith("component:"):
                out["components"][inner[10:]].add(n.attr)
        if isinstance(n, (ast.Assign, ast.AugAssign, ast.AnnAssign)):
            targets = n.targets if isinstance(n, ast.Assign) else [n.target]
            for t in targets:
                if isinstance(t, ast.Attribute):
                    k = handle_key(t.value)
                    kind = handles.get(k) if k else None
                    if kind in ("server", "config") or (kind or "").startswith("component:"):
                        out["writes"].add(f"{kind}.{t.attr}")
                    elif isinstance(t.value, ast.Name) and t.value.id in bound:
                        out["writes"].add(f"{bound[t.value.id]}.{t.attr}")
        if isinstance(n, ast.ClassDef):
            for b in n.bases:
                name = b.id if isinstance(b, ast.Name) else None
                if name in bound:
                    out["subclasses"].add(f"{n.name}({bound[name]})")
        if isinstance(n, ast.Call):
            kind = classify(n, handles)
            if kind and kind.startswith("component:"):
                out["components"].setdefault(kind[10:], set())
    return {k: (sorted(v) if isinstance(v, set) else {m: sorted(s) for m, s in sorted(v.items())})
            for k, v in out.items()}


def classify(value, handles):
    """What an expression holds: server, config, a named component, or None"""
    if isinstance(value, ast.Await):
        value = value.value
    if isinstance(value, ast.Call) and isinstance(value.func, ast.Attribute):
        f = value.func
        recv = handles.get(handle_key(f.value) or "")
        if f.attr == "get_server" and recv == "config":
            return "server"
        if f.attr in ("lookup_component", "load_component") and recv == "server":
            args = value.args[1:] if f.attr == "load_component" else value.args
            if args and isinstance(args[0], ast.Constant) and isinstance(args[0].value, str):
                return "component:" + args[0].value
    k = handle_key(value)
    return handles.get(k) if k else None


if __name__ == "__main__":
    json.dump({p: scan(p) for p in sys.argv[1:]}, sys.stdout, indent=1)
    print()
