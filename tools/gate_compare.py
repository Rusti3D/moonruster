#!/usr/bin/env python3
# The rustify gate's verdict (tools/rustify_gate.sh): compares a summary
# (tools/gate_summary.py) with the expected one. It fails when a case
# differs that did not differ in the expected run, or differs in a path the
# expected run did not, or a tool's RESULT / mqtt line changed. A case that
# differed in the expected run and no longer does passes (an improvement, or
# one of Moonraker's own volatile cases on a good run); it is listed.
#   python3 tools/gate_compare.py EXPECTED CURRENT
import sys


def parse(path):
    """{suite: (header lines, {case: set(paths)})}"""
    out, suite = {}, None
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("[") and line.endswith("]"):
            suite = line[1:-1]
            out[suite] = ([], {})
        elif line.startswith("  ") and ": " in line:
            case, paths = line[2:].split(": ", 1)
            out[suite][1].setdefault(case, set()).update(paths.split())
        elif suite is not None and line:
            out[suite][0].append(line)
    return out


def main():
    exp, cur = parse(sys.argv[1]), parse(sys.argv[2])
    fail, better = [], []
    for suite in sorted(set(exp) | set(cur)):
        if suite not in cur:
            fail.append(f"[{suite}] missing from this run")
            continue
        if suite not in exp:
            fail.append(f"[{suite}] not in the expected summary")
            continue
        (eh, ec), (ch, cc) = exp[suite], cur[suite]
        # tool verdicts (RESULT n differences, mqtt identical) must not change;
        # an 'N/M identical' total follows from the cases
        for e, c in zip(eh, ch):
            if e != c and "identical" not in e:
                fail.append(f"[{suite}] {e!r} -> {c!r}")
        for case, paths in sorted(cc.items()):
            if case not in ec:
                fail.append(f"[{suite}] new difference: {case}: {' '.join(sorted(paths))}")
            elif not paths <= ec[case]:
                fail.append(f"[{suite}] {case}: new paths {' '.join(sorted(paths - ec[case]))}")
        better += [f"[{suite}] {case}" for case in sorted(set(ec) - set(cc))]
    for b in better:
        print(f"now identical: {b}")
    for f in fail:
        print(f"FAIL {f}")
    print("GATE FAIL" if fail else "GATE PASS")
    return 1 if fail else 0


if __name__ == "__main__":
    sys.exit(main())
