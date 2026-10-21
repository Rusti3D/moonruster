#!/bin/bash
# Creates a local git origin + per-instance clones and a fake web client for update_manager tests
set -e
B=~/mr/umtest
rm -rf $B; mkdir -p $B
git init -q --bare -b master $B/origin.git
git clone -q $B/origin.git $B/work 2>/dev/null
cd $B/work
git config user.email t@t; git config user.name t
for i in 1 2 3; do echo "v$i" > f.txt; git add f.txt; git commit -qm "commit $i"; done
git tag v0.1.0
for i in 4 5; do echo "v$i" > f.txt; git commit -qam "commit $i"; done
git push -q origin master --tags
for n in ref rust; do
  D=~/mr/inst/$n
  rm -rf $D/client_repo $D/webclient
  git clone -q $B/origin.git $D/client_repo
  git -C $D/client_repo reset -q --hard HEAD~1
  mkdir -p $D/webclient
  echo '{"project_name": "mainsail", "project_owner": "mainsail-crew", "version": "v2.9.0"}' > $D/webclient/release_info.json
  sed -i '/^\[update_manager/,$d' $D/config/moonraker.conf
  cat >> $D/config/moonraker.conf <<CONF
[update_manager]
enable_system_updates: False

[update_manager testrepo]
type: git_repo
path: $D/client_repo
origin: $B/origin.git
is_system_service: False

[update_manager mainsail]
type: web
repo: mainsail-crew/mainsail
path: $D/webclient
CONF
done
echo ok
