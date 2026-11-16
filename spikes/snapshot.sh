#!/usr/bin/env bash
# Prototype: snapshot a worktree (tracked+staged+untracked, minus ignored)
# into refs/sync/<machine>/<branch> without touching branch, index or history.
# Usage: snapshot.sh <worktree> <machine>
set -euo pipefail
wt=$1; machine=$2
cd "$wt"
gitdir=$(git rev-parse --absolute-git-dir)
branch=$(git symbolic-ref --short HEAD)
ref="refs/sync/$machine/$branch"
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
cp "$gitdir/index" "$tmp" 2>/dev/null || true
export GIT_INDEX_FILE=$tmp
git add -A
tree=$(git write-tree)
prev=$(git rev-parse -q --verify "$ref" || true)
if [ -n "$prev" ] && [ "$(git rev-parse "$prev^{tree}")" = "$tree" ]; then echo "unchanged $prev"; exit 0; fi
parents=(-p HEAD); [ -n "$prev" ] && parents+=(-p "$prev")
commit=$(git commit-tree "$tree" "${parents[@]}" -m "gitsync snapshot $machine $(date -u +%FT%TZ)")
git update-ref "$ref" "$commit"
echo "$commit"
