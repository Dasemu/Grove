#!/usr/bin/env bash
# Phase 4 end-to-end check against a throwaway Forgejo on localhost:3300 (spikes/forgejo-up.sh):
# new repo -> config repo -> a fresh machine rebuilds the workspace with `grove bootstrap`.
set -euo pipefail
G=${GROVE_BIN:-target/debug/grove}
G=$(realpath "$G")
W=$(mktemp -d)
api() { curl -fs -u spike:spike-pass-123 "$@"; }
cleanup() {
  for r in alpha beta grove-config; do api -X DELETE "localhost:3300/api/v1/repos/spike/$r" >/dev/null || true; done
  rm -rf "$W"
}
trap cleanup EXIT
TOK=$(api -H 'Content-Type: application/json' -X POST localhost:3300/api/v1/users/spike/tokens \
  -d "{\"name\":\"e2e$RANDOM\",\"scopes\":[\"all\"]}" | sed -E 's/.*"sha1":"([^"]+)".*/\1/')
export GROVE_TOKEN_MAIN=$TOK
machine() { # name
  export XDG_CONFIG_HOME=$W/$1/cfg XDG_STATE_HOME=$W/$1/state
  "$G" init --machine "$1" --root "$W/$1/ws" --provider main --kind gitea \
    --url http://localhost:3300 --group work --config-repo grove-config >/dev/null
}
machine pca
"$G" new work alpha
"$G" new work beta --public
"$G" config-repo-init
machine pcb
"$G" bootstrap
test -d "$W/pcb/ws/work/alpha/main" && test -d "$W/pcb/ws/work/beta/main"
echo "OK: fresh machine pcb has both repos"
