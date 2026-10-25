#!/bin/bash
# Phase 0 trace instance (docs/PLUGINS.md): a separate Moonraker tree with the
# corpus components symlinked into moonraker/components (as their installers
# do), and instance ~/mr/inst/trace on port 7030. Leaves ~/mr/moonraker and
# the ref/rust instances alone.
# usage: setup_trace.sh [filaman]  (filaman: FilaMan replaces [spoolman]; they
# register the same endpoints, so only one of them can load)
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
CORPUS=/mnt/d/RustyKlipper/Originals_Base/addons/corpus
T=~/mr/trace
mkdir -p "$T"
[ -d "$T/moonraker" ] || git clone -q ~/mr/moonraker "$T/moonraker"
git -C "$T/moonraker" checkout -q 9e676eba
C="$T/moonraker/moonraker/components"
link() { ln -sfn "$CORPUS/$1" "$C/$(basename "$1")"; }
link moonraker-timelapse/component/timelapse.py
link uboe_moonraker_plugins/additional_pre_print_checks.py
link uboe_moonraker_plugins/spool_change_estimator.py
link uboe_moonraker_plugins/uboe_metadata.py
link Moonraker-3DGeeks-Component/geeks3d.py
link helixscreen/moonraker-plugin/helix_print.py
link FilaMan-Moonraker-Komponente/filaman.py
link Happy-Hare/components/mmu_server.py
bash "$HERE/../setup_instance.sh" trace 7030  # rewrites moonraker.conf and printer.cfg
D=~/mr/inst/trace
mkdir -p "$D/timelapse" "$D/timelapse_frames"
if [ "$1" = filaman ]; then
    SPOOL=$(printf '[filaman]\nserver: http://localhost:7912')
else
    SPOOL=$(printf '[spoolman]\nserver: http://localhost:7912\nsync_rate: 1')
fi
cat >> "$D/config/moonraker.conf" <<CONF

$SPOOL

[webcam cam]
stream_url: http://localhost:7913/stream
snapshot_url: http://localhost:7913/snapshot

[timelapse]
output_path: $D/timelapse
frame_path: $D/timelapse_frames

[uboe_metadata]

[additional_pre_print_checks]
work_hours_start: 08:00
work_hours_end: 20:00

[spool_change_estimator]

[geeks3d]

[helix_print]

[mmu_server]
CONF
echo "trace setup done: $T/moonraker, instance $D"
