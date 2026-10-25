#!/usr/bin/env python3
# Dumps the Python surface of the `moonraker` package as JSON, either of
# Moonraker itself or of Moonruster's mirror (docs/PLUGINS.md Phase 2), so
# the two can be compared name by name:
#  - every module's public and private names, with their kind
#  - functions and methods: signature, async / generator, staticmethod /
#    classmethod / property
#  - classes: bases (qualified), __module__, __qualname__
#  - module and class constants of plain types: their value
#
#   real:   python mirror_surface.py real MOONRAKER_REPO > real.json
#   mirror: python mirror_surface.py mirror MOONRUSTER_DIR MIRROR_OUT_DIR MOONRAKER_REPO [resolve] > mirror.json
#           (resolve: evaluate lazy names too; run it with Moonraker's venv)
#   diff:   python mirror_surface.py diff real.json mirror.json
# In mirror mode no third-party package may be imported (checked).
import importlib
import inspect
import json
import os
import sys
import types

PLAIN = (int, float, str, bool, bytes, type(None), tuple, frozenset)


def describe_callable(f):
    out = {"kind": "function"}
    try:
        out["sig"] = str(inspect.signature(f))
    except (TypeError, ValueError):
        out["sig"] = None
    out["async"] = inspect.iscoroutinefunction(f)
    out["gen"] = inspect.isgeneratorfunction(f)
    out["agen"] = inspect.isasyncgenfunction(f)
    return out


def describe_member(cls, name, raw):
    if isinstance(raw, staticmethod):
        return dict(describe_callable(raw.__func__), kind="staticmethod")
    if isinstance(raw, classmethod):
        return dict(describe_callable(raw.__func__), kind="classmethod")
    if isinstance(raw, property):
        return {"kind": "property", "set": raw.fset is not None}
    if inspect.isfunction(raw):
        return describe_callable(raw)
    if isinstance(raw, type):
        return {"kind": "class"}
    if type(raw).__name__ == "Attr" and type(raw).__module__ == "_mr":
        return {"kind": "instance_attr"}  # mirror: an instance attribute of Moonraker's class
    if isinstance(raw, PLAIN):
        return {"kind": "value", "value": repr(raw)}
    return {"kind": "object:" + type(raw).__name__}


def qualified(b):
    q = f"{b.__module__}.{b.__qualname__}"
    return "builtins.object" if q == "_mr.Object" else q  # the mirror's root class


def describe_class(cls):
    out = {"kind": "class", "module": cls.__module__, "qualname": cls.__qualname__,
           "bases": [qualified(b) for b in cls.__bases__], "members": {}}
    for name, raw in vars(cls).items():
        if name.startswith("__") and name.endswith("__") and name not in ("__init__", "__call__", "__aenter__",
                                                                         "__aexit__", "__enter__", "__exit__",
                                                                         "__iter__", "__aiter__", "__len__"):
            continue
        if name in ("__mr_mirror__", "_mr_h") or name.startswith("_mr_"):
            continue
        out["members"][name] = describe_member(cls, name, raw)
    try:
        import enum
        if issubclass(cls, enum.Enum):
            out["enum"] = {k: repr(v.value) for k, v in cls.__members__.items()}
    except Exception:
        pass
    return out


def describe_module(mod):
    def submodule(n, v):  # set on a package by importing its submodule, not a name it defines
        return isinstance(v, types.ModuleType) and v.__name__ == f"{mod.__name__}.{n}"
    out = {"__names__": {"kind": "names", "value": sorted(
        set(n for n, v in vars(mod).items()
            if not n.startswith("__") and not n.startswith("_mr_") and n != "_mr" and not submodule(n, v))
        | set(vars(mod).get("_mr_lazy", {})))}}
    for name, val in vars(mod).items():
        if name.startswith("__") or name.startswith("_mr_") or name in ("_mr",):
            continue
        if isinstance(val, types.ModuleType):
            continue  # imported modules: not part of the module's own surface
        if isinstance(val, type):
            if val.__module__ != mod.__name__:
                continue  # imported classes
            out[name] = describe_class(val)
        elif inspect.isfunction(val):
            if val.__module__ != mod.__name__:
                continue
            out[name] = describe_callable(val)
        elif isinstance(val, PLAIN):
            out[name] = {"kind": "value", "value": repr(val)}
    return out


def module_names(pkg_dir):
    names = []
    for root, dirs, files in os.walk(pkg_dir):
        dirs.sort()
        rel = os.path.relpath(root, os.path.dirname(pkg_dir)).replace(os.sep, ".")
        for f in sorted(files):
            if f.endswith(".py"):
                names.append(rel if f == "__init__.py" else rel + "." + f[:-3])
    return names


def dump(names):
    out, errors = {}, {}
    sys.argv = sys.argv[:1]
    for n in names:
        if n.endswith(".__main__"):
            continue  # the entry point runs Moonraker when imported
        try:
            out[n] = describe_module(importlib.import_module(n))
        except Exception as e:
            errors[n] = f"{type(e).__name__}: {e}"
    return {"modules": out, "errors": errors}


def main():
    mode = sys.argv[1]
    resolve = len(sys.argv) > 5 and sys.argv[5] == "resolve"
    if mode == "real":
        repo = sys.argv[2]
        sys.path.insert(0, repo)
        # third-party packages Moonraker imports with moonraker.utils, which
        # every Moonraker module imports: the mirror may import these too
        before = set(sys.modules)
        importlib.import_module("moonraker.utils")
        utils_third = sorted({m.split(".")[0] for m in set(sys.modules) - before}
                             - {"moonraker"} - set(sys.stdlib_module_names))
        res = dump(module_names(os.path.join(repo, "moonraker")))
        res["utils_third_party"] = utils_third
    elif mode == "mirror":
        mr_dir, out_dir, tree = sys.argv[2], sys.argv[3], sys.argv[4]
        mirror = os.path.join(out_dir, "mirror")
        modules = {}
        text = open(os.path.join(out_dir, "mirror_ids.rs")).read()
        block = text[text.index("pub static MODULES"):]
        for line in block[: block.index("];")].splitlines():
            line = line.strip()
            if line.startswith('("'):
                name, pkg = line.strip("(),").split(", ")
                modules[name.strip('"')] = pkg == "true"
        stub = types.ModuleType("_moonruster")
        stub.call = stub.fcall = stub.new = lambda *a, **k: None
        stub.get = lambda obj, id: None
        stub.set = stub.delete = lambda *a: None
        stub.patched_class = stub.patched_instance = lambda *a: None
        stub.source = lambda name: open(os.path.join(mirror, name + ".py"), encoding="utf-8").read()

        def module_value(modname, name):
            raise AttributeError(f"module {modname!r} has no attribute {name!r}")
        stub.module_value = module_value
        stub.log = lambda level, msg: None
        sys.modules["_moonruster"] = stub
        prelude = types.ModuleType("_mr")
        src = open(os.path.join(mr_dir, "src/plugins/prelude.py"), encoding="utf-8").read()
        exec(compile(src, "prelude.py", "exec"), prelude.__dict__)
        sys.modules["_mr"] = prelude
        # the reference tree: its paths become the mirror modules' __file__
        prelude.install(os.path.join(tree, "moonraker"), modules, 20)
        before = set(sys.modules)
        if resolve:
            # with the venv: evaluate every lazy name and class attribute, so
            # their values are compared too
            lazy_errors = {}
            for name in sorted(modules):
                if name.endswith(".__main__"):
                    continue
                try:
                    mod = importlib.import_module(name)
                except Exception:
                    continue
                for n in list(vars(mod).get("_mr_lazy", {})):
                    try:
                        getattr(mod, n)
                    except Exception as e:
                        lazy_errors[f"{name}.{n}"] = f"{type(e).__name__}: {e}"
                for cls in [v for v in list(vars(mod).values()) if isinstance(v, type)]:
                    for n, raw in list(vars(cls).items()):
                        if type(raw).__name__ == "LazyAttr":
                            try:
                                getattr(cls, n)
                            except Exception as e:
                                lazy_errors[f"{name}.{cls.__name__}.{n}"] = f"{type(e).__name__}: {e}"
        res = dump(sorted(modules))
        if resolve:
            res["errors"].update({k: "lazy: " + v for k, v in lazy_errors.items()})
        third = sorted({m.split(".")[0] for m in set(sys.modules) - before}
                       - {"moonraker", "_mr", "_moonruster"} - set(sys.stdlib_module_names))
        if not resolve:
            res["third_party_imported"] = third
    elif mode == "diff":
        real = json.load(open(sys.argv[2]))
        mirror = json.load(open(sys.argv[3]))
        return diff(real, mirror)
    json.dump(res, sys.stdout, indent=1, sort_keys=True)


def diff(real, mirror):
    problems = []
    for m, e in real["errors"].items():
        problems.append(f"real import error {m}: {e}")
    for m, e in mirror["errors"].items():
        problems.append(f"mirror import error {m}: {e}")
    extra = sorted(set(mirror.get("third_party_imported", [])) - set(real.get("utils_third_party", [])))
    if extra:
        problems.append(f"mirror imported third-party packages Moonraker's utils does not: {extra}")

    def cmp(path, a, b):
        if isinstance(a, dict) and isinstance(b, dict):
            for k in sorted(set(a) | set(b)):
                if k not in b:
                    problems.append(f"missing in mirror: {path}.{k}")
                elif k not in a:
                    if b[k] != {"kind": "instance_attr"}:
                        problems.append(f"extra in mirror: {path}.{k}")
                else:
                    cmp(f"{path}.{k}", a[k], b[k])
        elif a != b and not (isinstance(a, str) and "<object object at" in a):
            if path.endswith("__names__.value"):
                missing = sorted(set(a) - set(b))
                if missing:
                    problems.append(f"names missing in mirror: {path}: {missing}")
            else:
                problems.append(f"differs: {path}: real {a!r} mirror {b!r}")

    for m in sorted(real["modules"]):
        if m not in mirror["modules"]:
            problems.append(f"missing module {m}")
            continue
        cmp(m, real["modules"][m], mirror["modules"][m])
    for p in problems:
        print(p)
    print(f"{len(problems)} differences")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
