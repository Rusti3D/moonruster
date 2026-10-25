#!/usr/bin/env python3
# Test vectors from CPython for the smaller Python ports (docs/RUSTIFY.md
# Phase 0): shlex.split (shell_command.rs shlex_split), inspect.cleandoc
# (database.rs cleandoc).
#   python3 tools/gen_misc_vectors.py > crates/pycompat/tests/data/misc_vectors.json
import inspect
import json
import shlex
import sys

SHLEX = [
    "", "a", "a b  c", "a 'b c' \"d \\\"e\" f\\ g", "'single \\ backslash'", "\"double \\\\ \\$ \\` \\a\"",
    "a#b", "a #comment", "x=1 y='2 3'", "tab\tsep", "nl\nsep", "'unclosed", "\"unclosed", "trailing\\",
    "''", "\"\"", "a''b", "é ü", "--opt=\"a b\"", "  leading", "a\\'b", "\\\"q\\\"", "a;b", "a|b c&&d",
]

CLEANDOC = [
    "", "one line", "  leading", "first\n  second\n  third", "\n    table (\n        a INT,\n        b TEXT\n    )\n",
    "x\n\n    y\n\n", "\ttab\n\t\tdeeper", "  \n  only indent\n  ", "a\n b\n  c\n",
]


def split(s):
    try:
        return shlex.split(s)
    except ValueError as e:
        return {"error": str(e)}


def main():
    vec = {
        "shlex": [[s, split(s)] for s in SHLEX],
        "cleandoc": [[s, inspect.cleandoc(s)] for s in CLEANDOC],
    }
    json.dump(vec, sys.stdout, ensure_ascii=False, indent=0)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
