#!/usr/bin/env python3
# Test vectors for src/pyfmt.rs from CPython itself (docs/RUSTIFY.md Phase 0):
# repr() of floats and strings, int() / float() of strings, round(). The
# Rust unit test (pyfmt::tests::vectors) checks pyfmt against them.
#   python3 tools/gen_pyfmt_vectors.py > crates/pycompat/tests/data/pyfmt_vectors.json
import json
import math
import random
import struct
import sys

rnd = random.Random(20261025)


def floats():
    out = [0.0, -0.0, 1.0, -1.0, 0.1, 0.2, 0.3, 1 / 3, 2 / 3, 1e-4, 9.999e-5, 1e-5, 1e15, 1e16, 9.999999999999998e15,
           123456.789, 1.5e300, 5e-324, 2.2250738585072014e-308, 1.7976931348623157e308, 100.0, 1e22, 1e21, 0.5,
           2.5, 1234567890123456.0, 12345678901234567.0, 3.141592653589793, 299.99999999999994, 0.30000000000000004]
    for _ in range(400):
        bits = rnd.getrandbits(64)
        f = struct.unpack("<d", struct.pack("<Q", bits))[0]
        if math.isfinite(f):
            out.append(f)
    for _ in range(400):
        out.append(round(rnd.uniform(-1e6, 1e6), rnd.randint(0, 8)))
    for e in range(-30, 30):
        out.append(10.0 ** e)
        out.append(-1.5 * 10.0 ** e)
    return out


def strings():
    base = ["", "a", "it's", 'say "hi"', "both ' and \"", "back\\slash", "tab\there", "nl\nx", "cr\rx", "\x00\x01\x1f",
            "\x7f", "\x80\x9f", "\xa0", "\xad", "é", "日本", " ", "​", "퟿", "﻿", "\U0001F600",
            "\U000e0001", "", "\U0010ffff"]
    for _ in range(200):
        n = rnd.randint(0, 8)
        base.append("".join(chr(rnd.choice([rnd.randint(0x20, 0x7e), rnd.randint(0, 0x2ff),
                                            rnd.randint(0x2000, 0x206f), rnd.randint(0x1f300, 0x1f6ff)]))
                            for _ in range(n)))
    return [s for s in base if not any(0xd800 <= ord(c) <= 0xdfff for c in s)]


def int_float_inputs():
    return ["0", "1", "-1", "+5", " 7 ", "1_000", "1__0", "_1", "1_", "1.5", "", " ", "abc", "0x10", "١٢", "12e3",
            "1e3", "1.5e-3", "inf", "-Infinity", "nan", "NaN", ".5", "5.", "1_0.5", "1._5", "+-1", "--1", "9" * 30,
            "1e400", "-1e400", "0.0000001", "\t3.25\n", "1,5", "e5", "1e", "1e+", ".", "+.e1"]


def conv(fn, s):
    try:
        v = fn(s)
    except (ValueError, OverflowError):
        return None
    if isinstance(v, float) and not math.isfinite(v):
        return repr(v)
    return v


def main():
    fl = floats()
    vec = {
        "float_repr": [[struct.unpack("<Q", struct.pack("<d", f))[0], repr(f)] for f in fl],
        "str_repr": [[s, repr(s)] for s in strings()],
        "int": [[s, conv(int, s)] for s in int_float_inputs()],
        "float": [[s, conv(float, s)] for s in int_float_inputs()],
        "round": [[struct.unpack("<Q", struct.pack("<d", f))[0], n, repr(round(f, n))]
                  for f in fl[:300] for n in (0, 1, 2, 4) if math.isfinite(f) and abs(f) < 1e15],
    }
    json.dump(vec, sys.stdout, ensure_ascii=False, indent=0)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
