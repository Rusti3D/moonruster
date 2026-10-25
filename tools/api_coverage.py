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
src = "".join(open(f, encoding="utf-8").read() for f in glob.glob(str(ROOT / "src/plugins/impls/*.rs")))
handled = set(re.findall(r'"(moonraker\.[A-Za-z0-9_.]+)"', src))
for mac, prefix in re.findall(r'macro_rules! (\w+) \{\s*\(\$m:literal\) => \{\s*concat!\("([^"]+)"', src):
    handled |= {prefix + m for m in re.findall(mac + r'!\("([A-Za-z0-9_.]+)"\)', src)}
# names built in loops or field macros (prefix + literal list)
for prefix, body in re.findall(r'concat!\("([^"]+)", \$\w+, "([^"]+)"\)', src):
    pass
verbatim = {l.split("#")[0].strip() for l in open(ROOT / "tools/mirror_verbatim.txt") if l.split("#")[0].strip()}
LIFECYCLE = {"component_init", "close", "on_exit", "__init__"}


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
    tot_m += len(m); tot_mh += len(mh); tot_a += len(a); tot_ah += len(ah)
    miss = [n.split(".")[-1] for n in m if not ok(n)]
    if missing_only:
        if miss:
            print(f"{comp}: {' '.join(miss)}")
    else:
        print(f"{comp:22} methods {len(mh):3}/{len(m):<3} attrs {len(ah):3}/{len(a)}")
print(f"TOTAL methods {tot_mh}/{tot_m}, attributes {tot_ah}/{tot_a}")
