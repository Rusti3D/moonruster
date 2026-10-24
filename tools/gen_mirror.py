#!/usr/bin/env python3
# Generates the Python side of Moonruster's component layer (docs/PLUGINS.md
# "Mirror"): for every module of Moonraker's `moonraker` package a Python
# module with the same classes, methods (exact signatures and defaults),
# functions and constants, whose bodies call into Moonruster through the
# native module `_mr`.
#
# Derived from kaliru's tools/gen_mirror.py (@ b0e1435). Moonraker-specific:
#  - async methods await the call (`return await _mr.call(...)`), so they
#    stay coroutine functions; generators keep iterator semantics
#  - classes kept as Moonraker's own code ("verbatim"): exceptions, enums and
#    flags, dataclasses, classes with a stdlib or third-party base, and
#    classes deriving from one of those (transitively, across modules:
#    common.PkFlag derives from common.ExtendedFlag)
#  - the third-party packages Moonraker imports (THIRD_PARTY) are never
#    imported when a mirror module loads; module statements that import one
#    (eventloop's uvloop policy switch) are dropped
#
#  - mirrored class: `class X(<bases>):` whose methods are
#    `def m(self, a, b=1): return _mr.call(self, ID, a, b)`, `__init__` is
#    `_mr.new(self, ID, ...)` and instance attributes are _mr.Attr(ID) data
#    descriptors; the root base is _mr.Object
#  - literal module constants and class attributes are copied; other module
#    names resolve on first access through the module's __getattr__
#
# Output: OUT_DIR/mirror/<module>.py, OUT_DIR/mirror.bin (the modules,
# compressed) and OUT_DIR/mirror_ids.rs (ID -> qualified name).
#
# usage: gen_mirror.py MOONRAKER_REPO OUT_DIR
import ast
import builtins
import pathlib
import struct
import subprocess
import sys
import zlib

# third-party top-level packages Moonraker imports (AST scan of 9e676eba)
THIRD_PARTY = {"PIL", "apprise", "dbus_fast", "distro", "importlib_metadata", "inotify_simple", "jinja2",
               "ldap3", "libnacl", "lmdb", "msgspec", "paho", "periphery", "preprocess_cancellation",
               "serial", "streaming_form_data", "tornado", "uvloop", "zeroconf"}
# stdlib bases whose classes are kept as Moonraker's code
VERBATIM_ROOTS = {"Exception", "BaseException", "ValueError", "KeyError", "RuntimeError", "OSError",
                  "Enum", "Flag", "IntEnum", "IntFlag", "enum.Enum", "enum.IntEnum", "enum.Flag",
                  "NamedTuple", "TypedDict", "type"}
BUILTINS = set(dir(builtins)) | {"__file__", "__name__", "__package__", "__spec__"}
HERE = pathlib.Path(__file__).resolve().parent


def name_list(fname):
    return {l.split("#")[0].strip() for l in open(HERE / fname)} - {""}


# methods emitted as Moonraker's own code (one qualified name per line)
VERBATIM = name_list("mirror_verbatim.txt")
# classes on a third-party base that Moonruster implements: mirrored, the
# third-party base dropped (WebSocket, RPCHandler: request transports)
# "qualname: m1 m2" also names methods it inherits from a dropped base
# (SqliteProvider: Thread.is_alive)
NATIVE_CLASSES = {l.split(":")[0].strip(): l.split(":")[1].split() if ":" in l else []
                  for l in name_list("mirror_native_classes.txt")}
# every class Moonraker defines (a native class keeps only these as bases)
MOONRAKER_CLASSES = set()
VERBATIM_SEEN = set()
# whole modules emitted as Moonraker's own source: pure helpers without
# server state (utils.json_wrapper: its msgspec switch included)
VERBATIM_MODULES = name_list("mirror_verbatim_modules.txt")
# module statements with third-party side effects that Moonruster does not
# replay at startup: eventloop's uvloop policy (Moonruster drives the stdlib
# loop; docs/PLUGINS.md)
NO_STARTUP = {"moonraker.eventloop"}
FUTURE = "from __future__ import annotations"


def is_main_guard(node):
    return isinstance(node, ast.If) and "__main__" in ast.unparse(node.test)


def is_docstring(node):
    return isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant)


def bound_names(node):
    """names a statement binds (plain assignment targets, also inside try/if)"""
    return {s.id for s in ast.walk(node) if isinstance(s, ast.Name) and isinstance(s.ctx, ast.Store)}


def walk_runtime(node):
    """ast.walk without annotations (never evaluated: mirror modules use
    `from __future__ import annotations`, as Moonraker's do)"""
    todo = [node]
    while todo:
        n = todo.pop()
        yield n
        for field, value in ast.iter_fields(n):
            if field in ("annotation", "returns"):
                continue
            for v in value if isinstance(value, list) else [value]:
                if isinstance(v, ast.AST):
                    todo.append(v)


def statement_loads(node):
    """free names a statement loads"""
    loads = {n.id for n in walk_runtime(node) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load)}
    for sub in ast.walk(node):
        if isinstance(sub, ast.Lambda):
            loads -= {a.arg for a in sub.args.args + sub.args.kwonlyargs + sub.args.posonlyargs}
        elif isinstance(sub, ast.comprehension):
            loads -= {n.id for n in ast.walk(sub.target) if isinstance(n, ast.Name)}
        elif isinstance(sub, ast.ExceptHandler) and sub.name:
            loads.discard(sub.name)
    return loads - bound_names(node)


def imports_third_party(node):
    for n in ast.walk(node):
        if isinstance(n, ast.Import) and any(a.name.split(".")[0] in THIRD_PARTY for a in n.names):
            return True
        if isinstance(n, ast.ImportFrom) and n.level == 0 and (n.module or "").split(".")[0] in THIRD_PARTY:
            return True
    return False


def own_yields(fn):
    """whether fn itself (not a nested def) contains yield"""
    nested = {id(y) for m in ast.walk(fn) if m is not fn and isinstance(m, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda))
              for y in ast.walk(m)}
    return any(isinstance(y, (ast.Yield, ast.YieldFrom)) and id(y) not in nested for y in ast.walk(fn))


class Anchor:
    """In the output: the next line belongs at this line of Moonraker's file.
    Blank lines are added up to it when the output is not past it yet, so
    tracebacks through mirror code show Moonraker's line numbers and source
    lines (the module's __file__ is Moonraker's file)"""
    __slots__ = ("line",)

    def __init__(self, line):
        self.line = line


ALIGN_MISSES = []


def assemble(parts, modname):
    out = []
    for p in parts:
        if isinstance(p, Anchor):
            if len(out) + 1 < p.line:
                out.extend([""] * (p.line - 1 - len(out)))
            elif len(out) + 1 > p.line:
                ALIGN_MISSES.append((modname, p.line, len(out) + 1))
        else:
            out.extend(p.split("\n"))
    return "\n".join(out)


class Ids:
    def __init__(self):
        self.names = []
        self.index = {}

    def new(self, qualname):
        # one ID per name: a name defined twice (Python keeps the last
        # definition) is the same handler
        if qualname not in self.index:
            self.index[qualname] = len(self.names)
            self.names.append(qualname)
        return self.index[qualname]


def param_list(fn, src):
    """the def's parameter list as source text (annotations included: under
    `from __future__ import annotations` they are the same strings
    Moonraker's signatures show), and the call argument list"""
    a = fn.args

    def p_src(p, d=None):
        s = p.arg + (": " + ast.unparse(p.annotation) if p.annotation is not None else "")
        if d is not None:
            s += (" = " if p.annotation is not None else "=") + ast.get_source_segment(src, d)
        return s
    parts, call = [], []
    pos = a.posonlyargs + a.args
    defaults = [None] * (len(pos) - len(a.defaults)) + list(a.defaults)
    for i, (p, d) in enumerate(zip(pos, defaults)):
        parts.append(p_src(p, d))
        call.append(p.arg)
        if a.posonlyargs and i == len(a.posonlyargs) - 1:
            parts.append("/")
    if a.vararg:
        parts.append("*" + p_src(a.vararg))
        call.append("*" + a.vararg.arg)
    elif a.kwonlyargs:
        parts.append("*")
    for p, d in zip(a.kwonlyargs, a.kw_defaults):
        parts.append(p_src(p, d))
        call.append(f"{p.arg}={p.arg}")
    if a.kwarg:
        parts.append("**" + p_src(a.kwarg))
        call.append("**" + a.kwarg.arg)
    ret = f" -> {ast.unparse(fn.returns)}" if fn.returns is not None else ""
    return ", ".join(parts), call, ret


def targets(node):
    if isinstance(node, ast.Name):
        yield node.id
    elif isinstance(node, (ast.Tuple, ast.List)):
        for e in node.elts:
            yield from targets(e)


def names_in(node):
    return {n.id for n in ast.walk(node) if isinstance(n, ast.Name)}


def is_literal(node):
    try:
        ast.literal_eval(node)
        return True
    except Exception:
        return False


def definition_names(cls):
    """names a class statement evaluates when it runs: bases, keywords,
    decorators, class-body statements, and its methods' decorators and
    defaults (method bodies and annotations run later or never)"""
    nodes = list(cls.bases) + [k.value for k in cls.keywords] + list(cls.decorator_list)
    for n in cls.body:
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)):
            nodes += list(n.decorator_list) + list(n.args.defaults) + [d for d in n.args.kw_defaults if d]
        elif isinstance(n, ast.ClassDef):
            nodes.append(n)
        else:
            nodes.append(n)
    out = set()
    for n in nodes:
        out |= {x.id for x in walk_runtime(n) if isinstance(x, ast.Name) and isinstance(x.ctx, ast.Load)}
    return out


def base_names(cls):
    return [ast.unparse(b) for b in cls.bases] + [ast.unparse(k.value) for k in cls.keywords if k.arg == "metaclass"]


def is_dataclass(cls):
    return any(ast.unparse(d).split("(")[0].split(".")[-1] == "dataclass" for d in cls.decorator_list)


def verbatim_classes(trees):
    """names of classes kept as Moonraker's code, to a fixpoint across modules
    (not the NATIVE_CLASSES, which Moonruster implements)"""
    names = set()
    while True:
        before = len(names)
        for modname, tree, third in trees:
            for cls in [n for n in ast.walk(tree) if isinstance(n, ast.ClassDef)]:
                if f"{modname}.{cls.name}" in NATIVE_CLASSES:
                    continue
                bases = base_names(cls)
                if is_dataclass(cls) or any(
                        b in VERBATIM_ROOTS or b.split(".")[-1] in names or b.split(".")[0] in third
                        for b in bases):
                    names.add(cls.name)
        if len(names) == before:
            return names


def third_party_bound(tree):
    """names a module binds by importing a third-party package"""
    out = set()
    for n in ast.walk(tree):
        if isinstance(n, ast.Import):
            for a in n.names:
                if a.name.split(".")[0] in THIRD_PARTY:
                    out.add(a.asname or a.name.split(".")[0])
        elif isinstance(n, ast.ImportFrom) and n.level == 0 and (n.module or "").split(".")[0] in THIRD_PARTY:
            out |= {a.asname or a.name for a in n.names}
    return out


class ModuleGen:
    def __init__(self, modname, src, ids, verbatim_names, is_pkg=False, all_modules=()):
        self.modname = modname
        self.is_pkg = is_pkg
        self.all_modules = set(all_modules)
        self.src = src
        self.tree = ast.parse(src)
        self.ids = ids
        self.verbatim_names = verbatim_names
        self.imports = {}       # bound name -> import statement source
        self.cheap_names = set()  # bound by stdlib or mirror imports
        self.third_names = third_party_bound(self.tree)
        self.star_imports = []
        self.verbatim_used = []
        self.dropped = []       # statements neither emitted, lazy nor startup
        self.dropped_imports = set()
        self.eager_third_party = set()  # third-party names Moonraker's own code here uses
        self.lazy = {}          # name -> source run on first access
        self.startup = []       # third-party side effects (source)
        self.out = []
        self.attr_lines = []
        self.needed = set()     # names the emitted code references at definition time
        self.bound = set()      # module constants emitted so far

    def collect_imports(self):
        for node in self.tree.body:
            stmts = [node]
            if isinstance(node, (ast.If, ast.Try)):
                if isinstance(node, ast.If) and "TYPE_CHECKING" in ast.unparse(node.test):
                    continue  # typing-only imports: never executed
                stmts = [n for n in ast.walk(node) if isinstance(n, (ast.Import, ast.ImportFrom))]
            for n in stmts:
                if isinstance(n, ast.Import):
                    for a in n.names:
                        bound = a.asname or a.name.split(".")[0]
                        self.imports[bound] = ast.unparse(ast.Import(names=[a]))
                        if a.name.split(".")[0] not in THIRD_PARTY:
                            self.cheap_names.add(bound)
                elif isinstance(n, ast.ImportFrom):
                    for a in n.names:
                        if a.name == "*":
                            if n.level > 0:
                                self.star_imports.append(ast.unparse(n))
                            continue
                        bound = a.asname or a.name
                        self.imports[bound] = ast.unparse(ast.ImportFrom(module=n.module, names=[a], level=n.level))
                        if n.level > 0 or (n.module or "").split(".")[0] not in THIRD_PARTY:
                            self.cheap_names.add(bound)

    def is_data_statement(self, node):
        """an assignment or call statement that only uses builtins, cheap
        imports and names bound by earlier data statements"""
        if imports_third_party(node):
            return False
        if isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign)):
            if getattr(node, "value", None) is None:
                return False
            new = set(self.assigned_names(node)) if not isinstance(node, ast.AugAssign) else set()
        elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            new = set()
        else:
            return False
        return statement_loads(node) - new <= BUILTINS | self.cheap_names | self.bound

    def resolve_from(self, node, name):
        """the full module name `from <node.module> import name` would load
        if `name` is a submodule"""
        if node.level == 0:
            base = node.module or ""
        else:
            pkg = self.modname.split(".") if self.is_pkg else self.modname.split(".")[:-1]
            pkg = pkg[: len(pkg) - (node.level - 1)]
            base = ".".join(pkg + ([node.module] if node.module else []))
        return f"{base}.{name}"

    def binds(self, node):
        """module names a statement binds, imports included"""
        out = bound_names(node)
        for n in ast.walk(node):
            if isinstance(n, (ast.Import, ast.ImportFrom)):
                out |= {a.asname or a.name.split(".")[0] for a in n.names if a.name != "*"}
            elif isinstance(n, ast.comprehension):  # comprehension variables are local
                out -= {t.id for t in ast.walk(n.target) if isinstance(t, ast.Name)}
        return out

    def defer(self, node):
        """A statement the mirror cannot run at import (it needs a
        third-party package): a value-binding statement runs on first access
        of a name it binds (lazy); a statement with only side effects runs at
        startup (Moonraker ran it when it imported the module)."""
        src = ast.unparse(node)
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            names = {node.name}
        else:
            names = self.binds(node)
        if self.modname in NO_STARTUP:
            self.dropped.append(src.split("\n")[0])
            # its imports (uvloop) are not names of the module either
            self.dropped_imports |= self.binds(node) - bound_names(node)
            return
        if not names:
            # it runs once a module it imports is imported (prelude
            # run_startup): until a component uses that package, nobody can
            # observe the side effect, and the package is not loaded for it
            triggers = sorted({self.import_module_of(n) for n in statement_loads(node)
                               if n in self.imports and self.import_module_of(n)})
            self.startup.append((triggers, self.with_imports(node, src)))
            return
        code = self.with_imports(node, src)
        for name in names:
            self.lazy[name] = code

    def import_module_of(self, name):
        """the module a name's import statement imports (`from tornado.httpclient
        import AsyncHTTPClient`: tornado.httpclient), None for a relative one"""
        node = ast.parse(self.imports[name]).body[0]
        if isinstance(node, ast.ImportFrom):
            return node.module if node.level == 0 else None
        return node.names[0].name

    def with_imports(self, node, src):
        """src preceded by the imports and lazy statements it needs. It runs
        through exec(), which does not inherit the module's future import:
        it starts with its own, as Moonraker's modules do"""
        pre = [FUTURE]
        # names the statement imports itself (guarded imports) are not prefixed
        for name in sorted(statement_loads(node) - self.binds(node)):
            if name in self.lazy:
                dep = self.lazy[name].removeprefix(FUTURE + "\n")  # a future import must come first
                if dep not in pre:
                    pre.append(dep)
            elif name in self.imports and self.imports[name] not in pre:
                pre.append(self.imports[name])
        return "\n".join(pre + [src])

    @staticmethod
    def assigned_names(node):
        out = []
        for t in (node.targets if isinstance(node, ast.Assign) else [node.target]):
            out += list(targets(t))
        return out

    def emit_source(self, node, indent=""):
        """a node as Moonraker's source, re-indented, at its own lines"""
        lines = ast.get_source_segment(self.src, node, padded=False).split("\n")
        for d in getattr(node, "decorator_list", []):
            self.out.append(Anchor(d.lineno))
            self.out.append(indent + "@" + ast.get_source_segment(self.src, d))
        shift = len(indent) - node.col_offset
        self.out.append(Anchor(node.lineno))
        self.out.append(indent + lines[0])
        for l in lines[1:]:
            if not l.strip():
                self.out.append("")
            elif shift >= 0:
                self.out.append(" " * shift + l)
            else:
                self.out.append(l[-shift:])

    def emit_function(self, fn, indent, qual, method):
        if qual in VERBATIM:
            VERBATIM_SEEN.add(qual)
            self.ids.new(qual)  # IDs stay stable whichever way a method is emitted
            self.emit_source(fn, indent)
            self.verbatim_used.append(fn)
            return
        params, call, ret = param_list(fn, self.src)
        for d in fn.args.defaults + [d for d in fn.args.kw_defaults if d is not None]:
            self.needed |= names_in(d)
        for dec in fn.decorator_list:
            self.out.append(Anchor(dec.lineno))
            self.out.append(indent + "@" + ast.unparse(dec))
            self.needed |= names_in(dec)
        fid = self.ids.new(qual)
        is_async = isinstance(fn, ast.AsyncFunctionDef)
        decos = {ast.unparse(d) for d in fn.decorator_list}
        if method and fn.name == "__init__":
            first, _, rest = ", ".join(call).partition(", ")
            body = f"_mr.new({first}, {fid}{', ' + rest if rest else ''})"
        else:
            if method and not decos & {"staticmethod", "classmethod"}:
                first, _, rest = ", ".join(call).partition(", ")
                expr = f"_mr.call({first}, {fid}{', ' + rest if rest else ''})"
            else:
                args = ", ".join(call)
                expr = f"_mr.fcall({fid}{', ' + args if args else ''})"
            if own_yields(fn):
                body = f"async for _x in {expr}:\n{indent}        yield _x" if is_async else f"yield from {expr}"
            else:
                body = f"return await {expr}" if is_async else f"return {expr}"
        self.out.append(Anchor(fn.lineno))
        self.out.append(f"{indent}{'async def' if is_async else 'def'} {fn.name}({params}){ret}:")
        # the call at the original's first statement (after its docstring):
        # a traceback through the stub shows a line of Moonraker's method
        first = fn.body[1] if len(fn.body) > 1 and is_docstring(fn.body[0]) else fn.body[0]
        self.out.append(Anchor(first.lineno))
        self.out.append(f"{indent}    {body}")

    def emit_class(self, cls):
        bases_src = [ast.unparse(b).split("[")[0].split(".")[0] for b in cls.bases]
        native = f"{self.modname}.{cls.name}" in NATIVE_CLASSES
        if not native and any(b in self.lazy for b in bases_src):
            self.defer(cls)  # its base is defined on first access: so is it
            return
        qual = f"{self.modname}.{cls.name}"
        if cls.name in self.verbatim_names or qual in VERBATIM:
            if qual in VERBATIM:
                VERBATIM_SEEN.add(qual)
            if definition_names(cls) & self.third_names:
                # Moonraker's own class on a third-party base (mqtt's
                # ExtPahoClient): defined on first access
                self.defer(cls)
                return
            self.emit_source(cls)
            # Moonraker's own code looks its globals up in the module dict at
            # run time (no module __getattr__ there): every name it uses must
            # be bound (common.SqlTableDefType uses inspect.cleandoc).
            # Annotations are never evaluated, except that dataclasses looks
            # up ClassVar / InitVar (and typing / dataclasses) by name
            self.needed |= {n.id for n in walk_runtime(cls) if isinstance(n, ast.Name)}
            if is_dataclass(cls):
                self.needed |= names_in(cls) & {"ClassVar", "InitVar", "typing", "dataclasses"}
            self.out.append("")
            return
        bases = [ast.unparse(b) for b in cls.bases if ast.unparse(b) != "object"]
        # third-party bases (tornado's RequestHandler): a Moonruster object is
        # not one; the class roots at _mr.Object instead
        bases = [b for b in bases if b.split(".")[0] not in self.third_names | THIRD_PARTY]
        if native:
            # Moonruster's object: bases that exist only as Moonraker's tornado
            # code (defined on first access) are dropped as well
            bases = [b for b in bases if b.split("[")[0].split(".")[0] not in self.lazy]
            # and stdlib bases (SqliteProvider's Thread): the object is not one
            bases = [b for b in bases if b.split("[")[0].split(".")[-1] in MOONRAKER_CLASSES]
        for b in bases:
            self.needed |= names_in(ast.parse(b, mode="eval"))
        meta = [ast.unparse(k.value) for k in cls.keywords if k.arg == "metaclass"]
        if meta and meta[0].split(".")[-1] == "ABCMeta" or any(b.split(".")[-1] == "ABC" for b in bases):
            bases = [b for b in bases if b.split(".")[-1] != "ABC"] + ["metaclass=_mr.AbcMeta"]
        elif meta:
            raise SystemExit(f"{self.modname}.{cls.name}: metaclass {meta[0]} is not handled")
        self.out.append(Anchor(cls.lineno))
        self.out.append(f"class {cls.name}({', '.join(bases) or '_mr.Object'}):")
        qual = f"{self.modname}.{cls.name}"
        attrs, methods, local = set(), set(), set()
        for node in cls.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                methods.add(node.name)
                # a property's setter / deleter is a method of its own
                # (qualified name "<class>.<name>.setter")
                accessor = [d.attr for d in node.decorator_list
                            if isinstance(d, ast.Attribute) and d.attr in ("setter", "deleter")
                            and isinstance(d.value, ast.Name) and d.value.id == node.name]
                suffix = f".{accessor[0]}" if accessor else ""
                self.emit_function(node, "    ", f"{qual}.{node.name}{suffix}", True)
                if node.args.args:
                    me = node.args.args[0].arg
                    for sub in ast.walk(node):
                        if isinstance(sub, ast.Attribute) and isinstance(sub.ctx, ast.Store) \
                                and isinstance(sub.value, ast.Name) and sub.value.id == me:
                            attrs.add(sub.attr)
            elif isinstance(node, ast.ClassDef):
                self.emit_source(node, "    ")
                self.needed |= names_in(node)
                local.add(node.name)
            elif isinstance(node, (ast.Assign, ast.AnnAssign)) and node.value is not None:
                names = [t for x in (node.targets if isinstance(node, ast.Assign) else [node.target]) for t in targets(x)]
                if not imports_third_party(node) and (
                        is_literal(node.value)
                        or names_in(node.value) <= self.bound | local | self.cheap_names | BUILTINS):
                    self.out.append("    " + ast.unparse(node))
                    self.needed |= names_in(node.value)
                else:
                    # a class attribute that needs a third-party package
                    # (DbusManager.Variant = dbus_fast.Variant): Moonraker's
                    # statement, run on first access
                    code = self.with_imports(node, ast.unparse(node))
                    for name in names:
                        self.out.append(f"    {name} = _mr.LazyAttr({name!r}, {code!r})")
                local |= set(names)
        for m in NATIVE_CLASSES.get(qual, []):
            mid = self.ids.new(f"{qual}.{m}")
            self.out.append(f"    def {m}(self, *args, **kwargs):")
            self.out.append(f"        return _mr.call(self, {mid}, *args, **kwargs)")
            methods.add(m)
        # instance attributes: set on the class at the module's end (no lines
        # in between Moonraker's definitions)
        names = {a: self.ids.new(f"{qual}.{a}") for a in sorted(attrs - methods - local)}
        if names:
            self.attr_lines.append(f"_mr.attrs({cls.name}, {names!r})")
        if not cls.body or all(isinstance(n, ast.Expr) for n in cls.body) and not methods:
            self.out.append("    pass")
        self.out.append("")

    def generate(self):
        self.collect_imports()
        body, consts, later = [], [], []
        for node in self.tree.body:
            if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                body.append(node)
            elif self.is_data_statement(node):
                consts.append(node)
                if not isinstance(node, ast.Expr):
                    self.bound |= set(self.assigned_names(node))
            elif isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign, ast.Expr, ast.Try, ast.If)) \
                    and not is_main_guard(node) and not is_docstring(node) \
                    and not (isinstance(node, ast.If) and "TYPE_CHECKING" in ast.unparse(node.test)):
                if imports_third_party(node):
                    self.defer(node)
                else:
                    later.append(node)
        for c in consts:
            self.out.append(ast.unparse(c))
            self.needed |= names_in(c)
        self.out.append("")
        for node in body:
            if isinstance(node, ast.ClassDef):
                self.emit_class(node)
            else:
                self.emit_function(node, "", f"{self.modname}.{node.name}", False)
                self.out.append("")
            self.bound.add(node.name)
        defined = {n.name for n in body}
        for node in later:
            if statement_loads(node) <= BUILTINS | self.cheap_names | self.bound | defined:
                self.out.append(ast.unparse(node))
                self.needed |= names_in(node)
                self.bound |= bound_names(node)
            else:
                self.defer(node)
        for c in consts:
            if not isinstance(c, ast.Expr):
                defined |= set(self.assigned_names(c))
        head = [f"# Moonruster component layer: mirror of Moonraker {self.modname} (generated by tools/gen_mirror.py)",
                "from __future__ import annotations", "import _mr"]
        for name in sorted(self.needed - defined):
            if name in self.imports:
                if name not in self.cheap_names:
                    self.eager_third_party.add(name)
                head.append(self.imports[name])
        head += self.star_imports
        head.append("")
        module_names = BUILTINS | {"_mr", "__name__", "__file__"} | defined | self.bound
        module_names |= {n for n in self.imports if self.imports[n] in head}
        for fn in self.verbatim_used:
            local = {a.arg for a in fn.args.args + fn.args.kwonlyargs + fn.args.posonlyargs}
            local |= {x.arg for x in (fn.args.vararg, fn.args.kwarg) if x}
            local |= self.binds(fn) | {n.name for n in ast.walk(fn) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))}
            for sub in ast.walk(fn):
                if isinstance(sub, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda)):
                    sa = sub.args
                    local |= {x.arg for x in sa.args + sa.kwonlyargs + sa.posonlyargs}
                    local |= {x.arg for x in (sa.vararg, sa.kwarg) if x}
            free = statement_loads(ast.Module(body=fn.body, type_ignores=[])) - local - module_names
            for name in sorted(free):
                if name in self.imports:
                    head.append(self.imports[name])
                    module_names.add(name)
                else:
                    raise SystemExit(f"verbatim {self.modname}.{fn.name} uses {name!r}, which the mirror module does not bind")
        # every other name the module imports exists too, on first access
        # (components import re-exports: `from ..utils import ServerError`).
        # A submodule is imported by its full name: `from . import common`
        # would ask the package for `common` again, which is this entry
        for name, stmt in self.imports.items():
            if name in module_names or name in self.lazy or name in self.dropped_imports:
                continue
            node = ast.parse(stmt).body[0]
            full = self.resolve_from(node, name) if isinstance(node, ast.ImportFrom) else None
            if full in self.all_modules:
                stmt = f"import importlib as _mr_il\n{name} = _mr_il.import_module({full!r})"
            self.lazy[name] = stmt
        tail = [""] + self.attr_lines + [f"_mr_lazy = {self.lazy!r}", f"_mr_startup = {self.startup!r}",
                "__getattr__ = _mr.module_getattr(__name__, _mr_lazy)", ""]
        return assemble(head + self.out + tail, self.modname)


def main():
    repo, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    rev = subprocess.run(["git", "-C", str(repo), "rev-parse", "--short", "HEAD"],
                         capture_output=True, text=True).stdout.strip()
    sources = []
    for path in sorted((repo / "moonraker").rglob("*.py")):
        parts = list(path.relative_to(repo).with_suffix("").parts)
        is_pkg = parts[-1] == "__init__"
        if is_pkg:
            parts.pop()
        src = path.read_text(encoding="utf-8")
        sources.append((".".join(parts), is_pkg, src))
    trees = [(m, ast.parse(s), third_party_bound(ast.parse(s))) for m, _, s in sources]
    MOONRAKER_CLASSES.update(n.name for _, t, _ in trees for n in ast.walk(t) if isinstance(n, ast.ClassDef))
    verbatim_names = verbatim_classes(trees)
    ids = Ids()
    mdir = out / "mirror"
    mdir.mkdir(parents=True, exist_ok=True)
    for old in mdir.glob("*.py"):
        old.unlink()
    modules, dropped = [], []
    unknown = VERBATIM_MODULES - {m for m, _, _ in sources}
    if unknown:
        raise SystemExit(f"mirror_verbatim_modules.txt names Moonraker does not have: {sorted(unknown)}")
    for modname, is_pkg, src in sources:
        gen = ModuleGen(modname, src, ids, verbatim_names, is_pkg, [m for m, _, _ in sources])
        text = src if modname in VERBATIM_MODULES else gen.generate()
        compile(text, modname, "exec")
        (mdir / (modname + ".py")).write_text(text, encoding="utf-8", newline="\n")
        modules.append((modname, is_pkg))
        dropped += [f"{modname}: {d}" for d in gen.dropped]
        if gen.eager_third_party:
            print(f"  eager third-party imports in {modname}: {sorted(gen.eager_third_party)}", file=sys.stderr)
    # mirror.bin: u32 count, then per module (u16 name length, name, u32
    # compressed length, u32 source length, zlib stream); little endian
    with open(out / "mirror.bin", "wb") as f:
        f.write(struct.pack("<I", len(modules)))
        for m, _ in modules:
            src = (mdir / (m + ".py")).read_bytes()
            z = zlib.compress(src, 9)
            f.write(struct.pack("<H", len(m)) + m.encode() + struct.pack("<II", len(z), len(src)) + z)
    with open(out / "mirror_ids.rs", "w", encoding="utf-8", newline="\n") as f:
        f.write(f"// generated by tools/gen_mirror.py from Moonraker {rev}: do not edit\n")
        f.write("/// mirrored module names; true = a package\n")
        f.write("pub static MODULES: &[(&str, bool)] = &[\n")
        for m, p in modules:
            f.write(f'    ("{m}", {"true" if p else "false"}),\n')
        f.write("];\n\n/// qualified name of each method / function / attribute ID\n")
        f.write("pub static IDS: &[&str] = &[\n")
        for n in ids.names:
            f.write(f'    "{n}",\n')
        f.write("];\n")
    # component name -> the class its load_component() returns (lookup_component
    # hands Moonruster's built-in components to Python as these classes)
    srcs = {m: s for m, _, s in sources}

    def returned_class(modname):
        """(module, class) load_component() of `modname` returns: `return
        Class(config)`, or a package's `return fm.load_component(config)`
        with `from . import file_manager as fm`"""
        tree = ast.parse(srcs[modname])
        aliases = {}
        for n in tree.body:
            if isinstance(n, ast.ImportFrom) and n.level == 1 and n.module is None:
                for a in n.names:
                    aliases[a.asname or a.name] = f"{modname}.{a.name}"
        for n in tree.body:
            if isinstance(n, ast.FunctionDef) and n.name == "load_component":
                for r in ast.walk(n):
                    if isinstance(r, ast.Return) and isinstance(r.value, ast.Call):
                        f = r.value.func
                        if isinstance(f, ast.Name):
                            return modname, f.id
                        if isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) and f.value.id in aliases:
                            return returned_class(aliases[f.value.id])
        raise SystemExit(f"{modname}: no `return Class(config)` in load_component")

    comp_classes = []
    for modname, is_pkg, src in sources:
        if modname.startswith("moonraker.components.") and modname.count(".") == 2:
            comp_classes.append((modname.rsplit(".", 1)[1],) + returned_class(modname))
    # modules with third-party side effects (_mr_startup) must be modules
    # Moonraker imports at every start: those of the components server.py
    # always loads (CORE_COMPONENTS, SERVER_COMPONENTS, database)
    always = {"database"}
    for n in ast.parse(srcs["moonraker.server"]).body:
        if isinstance(n, ast.Assign) and getattr(n.targets[0], "id", "") in ("CORE_COMPONENTS", "SERVER_COMPONENTS"):
            always |= set(ast.literal_eval(n.value))
    startup_modules = []
    for modname, _, _ in sources:
        text = (mdir / (modname + ".py")).read_text(encoding="utf-8")
        if "\n_mr_startup = []" in text or modname in VERBATIM_MODULES:
            continue
        comp = next((c for c in always if modname == f"moonraker.components.{c}"
                     or modname.startswith(f"moonraker.components.{c}.")), None)
        if comp is None and not modname.startswith(("moonraker.server", "moonraker.common")):
            raise SystemExit(f"{modname} has startup side effects but is not a module Moonraker always imports")
        startup_modules.append(modname)
    with open(out / "mirror_ids.rs", "a", encoding="utf-8", newline="\n") as f:
        f.write("\n/// modules whose third-party side effects (_mr_startup) run when Python starts\n")
        f.write("pub static STARTUP_MODULES: &[&str] = &[\n")
        for m in startup_modules:
            f.write(f'    "{m}",\n')
        f.write("];\n")
        f.write("\n/// component name -> (module, class) of what its load_component() returns\n")
        f.write("pub static COMPONENT_CLASSES: &[(&str, &str, &str)] = &[\n")
        for n, m, c in comp_classes:
            f.write(f'    ("{n}", "{m}", "{c}"),\n')
        f.write("];\n")
    missing = VERBATIM - VERBATIM_SEEN
    if missing:
        raise SystemExit(f"mirror_verbatim.txt names Moonraker does not have: {sorted(missing)}")
    print(f"{len(modules)} modules, {len(ids.names)} ids, {len(VERBATIM_SEEN)} verbatim methods, "
          f"{len(verbatim_names)} verbatim classes", file=sys.stderr)
    for d in dropped:
        print(f"  dropped: {d}", file=sys.stderr)
    # definitions placed after their Moonraker line (generated lines before
    # them outnumber the original's): their tracebacks show other lines
    for modname, want, got in ALIGN_MISSES:
        print(f"  not at its line: {modname} line {want} (at {got})", file=sys.stderr)


if __name__ == "__main__":
    main()
