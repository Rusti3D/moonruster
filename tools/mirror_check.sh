#!/bin/bash
# Mirror completeness check (docs/PLUGINS.md Phase 2): generates the mirror
# from ref/moonraker and compares its Python surface with real Moonraker's.
#  - venv: every lazy name evaluated, every value compared: must be 0
#  - imports: loading every mirror module may import only the third-party
#    packages Moonraker itself imports with moonraker.utils
# usage (WSL, ~/mr setup): tools/mirror_check.sh
set -e
M="$(cd "$(dirname "$0")/.." && pwd)"
O=/tmp/mirror_check
PY=/root/mr/venv/bin/python
mkdir -p $O
cd /tmp
$PY -I $M/tools/gen_mirror.py $M/ref/moonraker $O/out 2> $O/gen.err || { cat $O/gen.err; exit 1; }
$PY $M/tools/mirror_surface.py real /root/mr/moonraker > $O/real.json
$PY -I $M/tools/mirror_surface.py mirror $M $O/out /root/mr/moonraker resolve > $O/venv.json
$PY -I $M/tools/mirror_surface.py mirror $M $O/out /root/mr/moonraker > $O/imports.json
$PY $M/tools/mirror_surface.py diff $O/real.json $O/venv.json > $O/venv.txt || true
$PY $M/tools/mirror_surface.py diff $O/real.json $O/imports.json > $O/imports.txt || true
echo "venv:    $(tail -1 $O/venv.txt)"
head -20 $O/venv.txt | grep -v "differences$" || true
echo "imports: $(grep -c "^mirror imported" $O/imports.txt || true) problem(s)"
grep "^mirror imported\|import error" $O/imports.txt || true
$PY -c "import json; print('third-party imported by all mirror modules:', json.load(open('$O/imports.json'))['third_party_imported'])"
