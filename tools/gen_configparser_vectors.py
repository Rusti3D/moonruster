#!/usr/bin/env python3
# Test vectors for src/configparser.rs from CPython's configparser, as
# Moonraker builds it (ConfigParser(interpolation=None)): for each text, the
# parsed sections (DEFAULT first, as dict(config.items())), write()'s output,
# or the error's str(). (docs/RUSTIFY.md Phase 0)
#   python3 tools/gen_configparser_vectors.py > tests/data/configparser_vectors.json
import configparser
import io
import json
import sys

CASES = [
    "",
    "[server]\nhost: 0.0.0.0\nport = 7125\n",
    "[a]\nKey: Value\nkey2 :  spaced value  \n",
    "[a]\nmulti:\n  one\n  two\n\n  three\nnext: x\n",
    "[a]\nk: v\n  continued\n# comment\n; also comment\nk2: v2\n",
    "[a]\nk: v # not a comment\n",
    "[DEFAULT]\nd: 1\n[a]\nk: 2\n[b]\nd: 3\n",
    "no section\n",
    "[a]\nk: 1\nk: 2\n",
    "[a]\n[a]\n",
    "[a]\nnovalue\n",
    "[a]\n=value\n",
    "[a]\nk=v=w\n",
    "[a]\nk: v:w\n",
    "[ spaced name ]\nx: 1\n",
    "[a]]\nx: 1\n",
    "[]\nx: 1\n",
    "  [a]\nx: 1\n",
    "[a]\n  x: 1\n",
    "[a]\nx:\n",
    "[a]\nx =\n",
    "[a]\nx: \t value with tab\t\n",
    "[a]\nUPPER: 1\nupper: 2\n",
    "[a]\nk: v\n\n\n[b]\n",
    "[a]\nk: line1\n\tline2\n",
    "[a]\r\nk: crlf\r\n",
    "[a]\nk: v\n[b\nx: 1\n",
    "[a]\nk: 1\n[b]\nk: 1\n[a]\nz: 2\n",
    "[gcode_macro test]\ngcode:\n  {% if x %}\n    G28\n  {% endif %}\n",
    "[a]\nk: unicode ✓ é\n",
    "#only comment\n",
    "[a]\n  # indented comment\nk: v\n",
    "[a]\nk: v\n   \n  after blank-ish\n",
]


def as_dict(cp):
    return {key: dict(val) for key, val in cp.items()}


def main():
    out = []
    for text in CASES:
        cp = configparser.ConfigParser(interpolation=None)
        try:
            cp.read_string(text, source="<cfg>")
        except Exception as e:
            out.append({"text": text, "error": str(e)})
            continue
        buf = io.StringIO()
        cp.write(buf)
        out.append({"text": text, "items": as_dict(cp), "sections": cp.sections(), "write": buf.getvalue()})
    json.dump(out, sys.stdout, ensure_ascii=False, indent=0)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
