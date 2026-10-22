# waits (max ~9 min) for a suite output file to contain its summary line
F=$1
for i in $(seq 1 108); do
  grep -qE "identical|BUILD FAILED" $F 2>/dev/null && break
  pgrep -f "tools/run_all.sh" > /dev/null || break
  sleep 5
done
tail -${2:-80} $F
