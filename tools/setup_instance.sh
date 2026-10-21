#!/bin/bash
# Creates a test instance (data folder, moonraker.conf, printer.cfg)
# usage: setup_instance.sh <name> <port>
set -e
NAME=$1
PORT=$2
D=~/mr/inst/$NAME
mkdir -p "$D/config" "$D/logs" "$D/gcodes" "$D/database" "$D/comms"
cat > "$D/config/moonraker.conf" <<CONF
[server]
host: 0.0.0.0
port: $PORT
klippy_uds_address: $D/comms/klippy.sock

[authorization]
trusted_clients:
  127.0.0.0/8
  ::1/128
cors_domains:
  *.example.com

[machine]
provider: none

[history]

[octoprint_compat]

[job_queue]

[data_store]
CONF
python3 - "$D" <<'PY'
import re, sys, pathlib
d = sys.argv[1]
src = pathlib.Path.home().joinpath("mr/moonraker/tests/assets/klipper/base_printer.cfg").read_text()
keys = {"gcode_path": f"{d}/gcodes", "klippy_uds_path": f"{d}/comms/klippy.sock"}
out = re.sub(r"\$\{([^}]+)\}", lambda m: keys.get(m.group(1), m.group(0)), src)
# options removed from current klipper
out = "\n".join(l for l in out.splitlines() if not l.startswith("relative_reference_index")) + "\n"
pathlib.Path(d, "config/printer.cfg").write_text(out)
PY
echo "instance $NAME ready at $D"
