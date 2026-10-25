#!/usr/bin/env python3
# The rustify gate's summary of harness outputs (docs/RUSTIFY.md "Gates"):
# per suite, its total line and, per differing case, the set of JSON paths
# that differ, without their values or list indices. Values of a known
# difference may vary between runs (durations, tokens, Moonraker's random
# component order); a new differing field or case changes the summary.
#   python3 tools/gate_summary.py DIR > summary   (DIR: <suite>.out files)
import pathlib
import re
import sys

PATH = re.compile(r"^\s+(\$[^\s:]*)")


def strip_indices(path):
    return re.sub(r"\[\d+\]", "[]", path)


def harness(text):
    """=== case / indented diff lines / 'N/M ... identical'"""
    out, case, paths, total = [], None, set(), ""

    def flush():
        if case is not None:
            out.append(f"  {case}: {' '.join(sorted(paths)) or '(differs)'}")

    for line in text.splitlines():
        if line.startswith("=== "):
            flush()
            case, paths = line[4:].strip(), set()
        elif case is not None and (m := PATH.match(line)):
            paths.add(strip_indices(m.group(1)))
        elif re.match(r"^\d+/\d+ .*identical", line):
            total = line.strip()
    flush()
    return [total or "(no total)"] + sorted(out)


def cmp_tool(text):
    """plugin_cmp / corpus_cmp: DIFF lines and the RESULT line"""
    diffs = sorted({line.split(":")[0].strip() for line in text.splitlines() if line.startswith("DIFF ")})
    result = next((line.strip() for line in text.splitlines() if line.startswith("RESULT")), "(no RESULT)")
    return [result] + [f"  {d}" for d in diffs]


def mqtt_tls(text):
    """ref and rust replies: identical or not"""
    blocks, cur = {}, None
    for line in text.splitlines():
        if line.startswith("=== "):
            cur = line[4:].strip()
        elif cur and '"jsonrpc"' in line:
            blocks.setdefault(cur, []).append(line.strip())
    same = blocks.get("ref") == blocks.get("rust") and blocks.get("ref")
    return ["identical" if same else f"differs: {blocks}"]


def main():
    d = pathlib.Path(sys.argv[1])
    for f in sorted(d.glob("*.out")):
        name = f.stem
        text = f.read_text(errors="replace")
        if name in ("plugin_cmp", "corpus_cmp"):
            lines = cmp_tool(text)
        elif name == "mqtt_tls":
            lines = mqtt_tls(text)
        else:
            lines = harness(text)
        print(f"[{name}]")
        for line in lines:
            print(line)


if __name__ == "__main__":
    main()
