#!/usr/bin/env bash
# Start a throwaway Forgejo (dev only) on localhost:3300 with an admin user.
set -euo pipefail
podman rm -f grove-forgejo >/dev/null 2>&1 || true
podman run -d --name grove-forgejo -p 3300:3000 \
  -e FORGEJO__security__INSTALL_LOCK=true \
  -e FORGEJO__server__ROOT_URL=http://localhost:3300/ \
  -e FORGEJO__server__DISABLE_SSH=true \
  -e FORGEJO__database__DB_TYPE=sqlite3 \
  -e FORGEJO__mirror__ENABLED=true -e FORGEJO__migrations__ALLOW_LOCALNETWORKS=true -e FORGEJO__migrations__ALLOWED_DOMAINS=localhost \
  codeberg.org/forgejo/forgejo:11 >/dev/null
for i in $(seq 60); do curl -fs localhost:3300/api/healthz >/dev/null && break; sleep 1; done
podman exec --user git grove-forgejo forgejo admin user create --admin \
  --username spike --password spike-pass-123 --email spike@example.invalid --must-change-password=false
