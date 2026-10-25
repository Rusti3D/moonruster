#!/bin/bash
# Where binary size goes: builds two checkouts in release with symbols (v0
# mangling, so generic instances are told apart) and diffs symbol sizes by
# demangled name, then by leading crate.
#   tools/size_diff.sh /mnt/d/RustyKlipper/Moonruster /mnt/d/RustyKlipper/Moonruster-rustify
# Takes the machine's gate lock: two full builds.
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    export RUSTIFY_GATE_LOCKED=1
    exec flock -o /tmp/rustify_gates.lock bash "$0" "$@"
fi
[ $# -eq 2 ] || { echo "usage: $0 <old checkout> <new checkout>"; exit 1; }
export CARGO_PROFILE_RELEASE_STRIP=false
export RUSTFLAGS="-C symbol-mangling-version=v0"
OUT=~/mr/sizediff
mkdir -p $OUT
for name in old new; do
    [ $name = old ] && dir=$1 || dir=$2
    (cd "$dir" && CARGO_TARGET_DIR=~/mr/target_sym_$name ~/.cargo/bin/cargo build --release 2>&1 | grep -E "^error|Finished")
    bin=~/mr/target_sym_$name/release/moonruster
    echo "$name $(size -A $bin | grep -E '^\.text')"
    nm -S --size-sort -C $bin | awk '{s=strtonum("0x"$2); $1="";$2="";$3=""; print s"\t"$0}' > $OUT/$name.txt
done
python3 - "$OUT" <<'EOF'
import collections, re, sys


def load(n):
    d = collections.defaultdict(lambda: [0, 0])
    for l in open('%s/%s.txt' % (sys.argv[1], n)):
        s, name = l.rstrip('\n').split('\t', 1)
        name = re.sub(r'\[[0-9a-f]+\]', '', name.strip())
        d[name][0] += int(s)
        d[name][1] += 1
    return d


a, b = load('old'), load('new')
print('symbols total', sum(v[0] for v in a.values()), sum(v[0] for v in b.values()))
delta = sorted((b.get(k, [0, 0])[0] - a.get(k, [0, 0])[0], a.get(k, [0, 0])[1], b.get(k, [0, 0])[1], k)
               for k in set(a) | set(b))
print('--- grew (bytes, copies before, copies after, name)')
for d in reversed(delta[-30:]):
    print(d[0], d[1], d[2], d[3][:200])
print('--- shrank')
for d in delta[:30]:
    print(d[0], d[1], d[2], d[3][:200])
crate = lambda k: (re.match(r'^<*([a-z_0-9]+)::', k) or [None, '?'])[1]
ca, cb = collections.Counter(), collections.Counter()
for k, v in a.items():
    ca[crate(k)] += v[0]
for k, v in b.items():
    cb[crate(k)] += v[0]
print('--- by leading crate')
for k in sorted(set(ca) | set(cb), key=lambda k: -abs(cb[k] - ca[k]))[:20]:
    if cb[k] != ca[k]:
        print(cb[k] - ca[k], ca[k], cb[k], k)
print('extra copies of an existing name:', sum(d[0] for d in delta if d[2] > d[1] > 0))
EOF
