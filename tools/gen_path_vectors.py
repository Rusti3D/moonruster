#!/usr/bin/env python3
# Test vectors for pycompat's posixpath and urllib modules from
# CPython (docs/RUSTIFY.md Phase 1): os.path.normpath / join / splitext /
# basename / dirname / relpath (absolute arguments: no cwd involved) and
# urllib.parse.quote / unquote / urlencode.
#   python3 tools/gen_path_vectors.py > tests/data/path_vectors.json
import json
import posixpath
import sys
import urllib.parse

PATHS = ["", ".", "..", "/", "//", "///", "a", "a/", "/a", "//a", "///a", "a/b/../c", "a/../..", "../a", "/../a",
         "/a/./b/", "a//b", "/a/b/../../..", "./a", "a/.", "a/..", "//a/b/../..", "a/b/c/../../d", "~/x/../y",
         "/a/b c/d", "/é/ü/../ö", "a/..b/.c", "....", "/./.", "a/./../b"]
SPLIT = ["a.txt", "a.tar.gz", ".bashrc", "a/.bashrc", "a.b/c", "a/.b.c", "foo..bar", "foo.", "...", ".a.", "a/b.",
         "/x/y.gcode", "noext", "", ".", "..", "a/..b", "dir.d/file", "C:\\x.y"]
JOIN = [["a", "b"], ["a/", "b"], ["a", "/b"], ["", "b"], ["a", ""], ["/", "x"], ["a", "b", "c"], ["a", "/b", "c"],
        ["a//", "b"], ["/x/y", "../z"]]
REL = [["/a/b/c", "/a"], ["/a", "/a/b/c"], ["/a/b", "/a/b"], ["/a/b", "/c/d"], ["/", "/a"], ["/a", "/"],
       ["/a/b/../c", "/a"], ["/a//b", "/a"], ["/x/y z", "/x"]]
QUOTE = ["", "abc", "a b", "a/b", "a+b&c=d", "é", "日本", "~._-", "100%", "a?b#c", "/path/with space/ü.gcode",
         "!*'();:@&=+$,/?#[]", "\x00\x7f"]


def main():
    vec = {
        "normpath": [[p, posixpath.normpath(p)] for p in PATHS],
        "splitext": [[p, list(posixpath.splitext(p))] for p in SPLIT + PATHS],
        "basename": [[p, posixpath.basename(p)] for p in PATHS + SPLIT],
        "dirname": [[p, posixpath.dirname(p)] for p in PATHS + SPLIT],
        "join": [[j, posixpath.join(*j)] for j in JOIN],
        "relpath": [[r, posixpath.relpath(*r)] for r in REL],
        "quote": [[s, urllib.parse.quote(s)] for s in QUOTE],
        "quote_nosafe": [[s, urllib.parse.quote(s, safe="")] for s in QUOTE],
        "unquote": [[urllib.parse.quote(s, safe=""), s] for s in QUOTE]
        + [[s, urllib.parse.unquote(s)] for s in ["%", "%2", "%zz", "a%20b", "%E9", "%C3%A9", "+"]],
        "urlencode": [[[["a b", "c&d"], ["é", "x/y"]], urllib.parse.urlencode([("a b", "c&d"), ("é", "x/y")])]],
    }
    json.dump(vec, sys.stdout, ensure_ascii=False, indent=0)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
