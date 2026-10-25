# Runs the differential suites one after another, results in /tmp/mr_*.out
# usage: run_all.sh [suite...]  (default: all)
# the machine's gate lock (kaliru's gates measure resources here): taken by
# a flock process for this script's lifetime, with the lock fd closed (-o)
# in the script, so nothing it starts (test servers) can inherit and keep it
if [ -z "$RUSTIFY_GATE_LOCKED" ]; then
    export RUSTIFY_GATE_LOCKED=1
    exec flock -o /tmp/rustify_gates.lock bash "$0" "$@"
fi
cd "$(dirname "$0")/.."
CARGO_TARGET_DIR=~/mr/target ~/.cargo/bin/cargo build --release 2>&1 | grep -E "^error" -A12 > /tmp/mr_build.out
if [ -s /tmp/mr_build.out ]; then echo "BUILD FAILED" > /tmp/mr_done.out; cat /tmp/mr_build.out >> /tmp/mr_done.out; exit 1; fi
cd tools
SUITES=${@:-power machine_cli machine_dbus mqtt_tls config scenario ext difftest}
bash reset_both.sh > /dev/null
for s in $SUITES; do
  case $s in
    power) timeout 900 bash power_test.sh ;;
    machine_cli) timeout 900 bash machine_test.sh systemd_cli ;;
    machine_dbus) timeout 900 bash machine_test.sh systemd_dbus ;;
    mqtt_tls) timeout 600 bash mqtt_tls_test.sh ;;
    ldap) timeout 600 bash ldap_test.sh ;;
    ppc) timeout 600 bash ppc_test.sh ;;
    config) timeout 900 ~/mr/venv/bin/python config_cmp.py ;;
    scenario) bash reset_both.sh > /dev/null; timeout 1800 ~/mr/venv/bin/python scenario.py ;;
    ext) bash reset_both.sh > /dev/null; timeout 600 ~/mr/venv/bin/python ext_test.py ;;
    difftest) bash reset_both.sh > /dev/null; timeout 900 ~/mr/venv/bin/python difftest.py ;;
  esac > /tmp/mr_$s.out 2>&1
done
bash reset_both.sh > /dev/null
echo "$SUITES" > /tmp/mr_done.out
