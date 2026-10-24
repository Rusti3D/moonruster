#!/usr/bin/env python3
# Runs Python Moonraker with its objects instrumented (docs/PLUGINS.md
# Phase 0) and records, as JSON lines in $TRACE_OUT (first occurrence only):
#  - "use": a third-party component reads or writes an attribute of a
#    Moonraker object, class or module
#  - "call": a component calls a registration / lookup / event function of
#    Moonraker (with its str arguments)
#  - "back": Moonraker reads an attribute of an object a component defined
#    (the protocol Moonraker expects from component objects)
# Third-party components are the module names in $TRACE_COMPONENTS
# (comma separated, e.g. "timelapse,mmu_server").
#
# usage: TRACE_OUT=f TRACE_COMPONENTS=a,b tracer.py MOONRAKER_DIR [moonraker args]
import enum
import importlib.abc
import importlib.machinery
import json
import os
import sys
import types

OUT = open(os.environ["TRACE_OUT"], "a", buffering=1)
COMPONENTS = {"moonraker.components." + n for n in os.environ["TRACE_COMPONENTS"].split(",") if n}
SEEN = set()
CALL_LOG = {"lookup_component", "load_component", "register_component", "send_event",
            "register_event_handler", "register_notification", "register_remote_method",
            "register_endpoint", "register_debug_endpoint", "register_static_file_handler",
            "register_upload_handler", "register_directory", "register_local_namespace",
            "wrap_namespace", "register_feed", "register_auxiliary_field", "call_method"}


def caller_kind(depth):
    name = sys._getframe(depth).f_globals.get("__name__", "")
    if name in COMPONENTS or any(name.startswith(c + ".") for c in COMPONENTS):
        return "component", name
    if name == "moonraker" or name.startswith("moonraker."):
        return "moonraker", name
    return None, name


def emit(rec):
    key = json.dumps(rec, sort_keys=True)
    if key not in SEEN:
        SEEN.add(key)
        OUT.write(key + "\n")


def owner_name(obj):
    t = obj if isinstance(obj, type) else type(obj)
    return f"{t.__module__}.{t.__qualname__}"


def logged_call(fn, owner, name, comp):
    def wrapper(*args, **kw):
        strs = [a for a in args if isinstance(a, str)]
        emit({"kind": "call", "owner": owner, "name": name, "args": strs[:2], "by": comp})
        return fn(*args, **kw)
    return wrapper


def instrument_moonraker_class(cls):
    def getattribute(self, name):
        val = object.__getattribute__(self, name)
        kind, who = caller_kind(2)
        if kind == "component":
            owner = owner_name(self)
            emit({"kind": "use", "owner": owner, "name": name, "op": "get", "by": who})
            if name in CALL_LOG and callable(val):
                return logged_call(val, owner, name, who)
        return val

    def setattr_(self, name, value):
        kind, who = caller_kind(2)
        if kind == "component":
            emit({"kind": "use", "owner": owner_name(self), "name": name, "op": "set", "by": who})
        object.__setattr__(self, name, value)

    cls.__getattribute__ = getattribute
    cls.__setattr__ = setattr_


def instrument_component_class(cls):
    def getattribute(self, name):
        kind, who = caller_kind(2)
        if kind == "moonraker" and not name.startswith("__"):
            emit({"kind": "back", "owner": owner_name(self), "name": name, "by": who})
        return object.__getattribute__(self, name)

    cls.__getattribute__ = getattribute


class TracedModule(types.ModuleType):
    def __getattribute__(self, name):
        kind, who = caller_kind(2)
        if kind == "component" and not name.startswith("__"):
            emit({"kind": "use", "owner": types.ModuleType.__getattribute__(self, "__name__"),
                  "name": name, "op": "get", "by": who})
        return types.ModuleType.__getattribute__(self, name)

    def __setattr__(self, name, value):
        kind, who = caller_kind(2)
        if kind == "component":
            emit({"kind": "use", "owner": types.ModuleType.__getattribute__(self, "__name__"),
                  "name": name, "op": "set", "by": who})
        types.ModuleType.__setattr__(self, name, value)


def instrument_module(mod):
    name = mod.__name__
    is_comp = name in COMPONENTS or any(name.startswith(c + ".") for c in COMPONENTS)
    for v in list(vars(mod).values()):
        if not isinstance(v, type) or v.__module__ != name:
            continue
        if issubclass(v, (enum.Enum, BaseException, type)) or "__slots__" in vars(v):
            continue  # enum members / exceptions / metaclasses / slotted: static scan covers them
        if is_comp:
            instrument_component_class(v)
        else:
            instrument_moonraker_class(v)
    if not is_comp:
        mod.__class__ = TracedModule


class Loader(importlib.abc.Loader):
    def __init__(self, inner):
        self.inner = inner

    def __getattr__(self, name):  # get_resource_reader, get_data, ... (importlib.resources)
        return getattr(self.inner, name)

    def create_module(self, spec):
        return self.inner.create_module(spec)

    def exec_module(self, module):
        self.inner.exec_module(module)
        instrument_module(module)


class Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path, target=None):
        if fullname != "moonraker" and not fullname.startswith("moonraker."):
            return None
        spec = importlib.machinery.PathFinder.find_spec(fullname, path)
        if spec is not None and spec.loader is not None:
            spec.loader = Loader(spec.loader)
        return spec


if __name__ == "__main__":
    mr_dir = sys.argv[1]
    sys.argv = [os.path.join(mr_dir, "moonraker", "moonraker.py")] + sys.argv[2:]
    sys.path.insert(0, mr_dir)
    sys.meta_path.insert(0, Finder())
    import importlib
    importlib.import_module("moonraker.server").main(False)
