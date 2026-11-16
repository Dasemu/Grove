#!/usr/bin/env bash
# Phase 0 spike against a local throwaway Forgejo (run spikes/forgejo-up.sh first).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
API=http://localhost:3300/api/v1; AUTH=spike:spike-pass-123
w=$(mktemp -d); cd "$w"
curl -fs -u $AUTH -H 'Content-Type: application/json' -X POST $API/user/repos \
  -d '{"name":"demo","private":true}' >/dev/null
curl -fs -u $AUTH -H 'Content-Type: application/json' -X POST $API/user/repos \
  -d '{"name":"demo-mirror","private":true}' >/dev/null
URL=http://spike:spike-pass-123@localhost:3300/spike/demo.git
git init -q -b main m1 && cd m1
git config user.email a@b.c; git config user.name t
echo hi > a.txt; git add a.txt; git commit -qm init
git remote add origin $URL; git push -q origin main
echo "wip" > untracked.txt; echo "edit" >> a.txt; echo secret > ignored.env; echo "*.env" > .gitignore
c=$("$here/snapshot.sh" . desktop); echo "snapshot: $c"
echo "--- push refs/sync (force)"
git push -f origin "refs/sync/desktop/main:refs/sync/desktop/main" 2>&1 | tail -3
echo "--- ls-remote"; git ls-remote origin
echo "--- API branches"; curl -fs -u $AUTH $API/repos/spike/demo/branches | python3 -c 'import sys,json;print([b["name"] for b in json.load(sys.stdin)])'
echo "--- API git refs"; curl -fs -u $AUTH "$API/repos/spike/demo/git/refs" | python3 -c 'import sys,json;print([r["ref"] for r in json.load(sys.stdin)])'
echo "--- 2nd machine fetch + restore"
cd "$w"; git clone -q $URL m2 && cd m2
git fetch -q origin "+refs/sync/*:refs/sync/*"; git for-each-ref refs/sync
git show --stat refs/sync/desktop/main | head -12
git ls-tree -r --name-only refs/sync/desktop/main
echo "--- push mirror to demo-mirror"
curl -s -u $AUTH -H 'Content-Type: application/json' -X POST $API/repos/spike/demo/push_mirrors \
  -d '{"remote_address":"http://localhost:3000/spike/demo-mirror.git","remote_username":"spike","remote_password":"spike-pass-123","interval":"8h","sync_on_commit":true}' | head -c 300; echo
sleep 1; curl -s -u $AUTH -X POST $API/repos/spike/demo/mirror-sync -o /dev/null -w "mirror-sync http=%{http_code}\n"
cd "$w/m1"; echo more >> a.txt; git commit -qam second; git push -q origin main; sleep 8
echo "--- mirror refs"; git ls-remote http://spike:spike-pass-123@localhost:3300/spike/demo-mirror.git
echo "workdir: $w"
