# Phase 0 spike: custom refs on Gitea/Forgejo

Scripts: `spikes/forgejo-up.sh` (throwaway Forgejo 11 via Podman), `spikes/snapshot.sh`
(plumbing-only snapshot), `spikes/phase0.sh` (end-to-end checks).

## Results (Forgejo 11, local, 2026-11-16)

| Question | Result |
|---|---|
| Push `refs/sync/<machine>/<branch>` (force) | Accepted |
| Appears as a branch (API `/branches`) | No, only `main` |
| Visible via API `/git/refs` | Yes (so not fully hidden; UI not checked visually) |
| Fetchable from another clone (`+refs/sync/*:refs/sync/*`) | Yes; tree contains tracked, staged and untracked files, excludes ignored |
| Snapshot leaves branch, index and history untouched | Yes (temp `GIT_INDEX_FILE`) |
| Included by push mirror | No, mirror received only `refs/heads/main` (+HEAD) |

Conclusion: **no fallback needed** for Forgejo. Section 7.1 design holds.

## Real instance (git.zenvairo.es, Gitea 1.25.4, 2026-11-16)
Throwaway private repo, since deleted. Force push of `refs/sync/desktop/main` accepted; `tea branches list` shows only `main`; a second clone fetched the ref and the tree held tracked and untracked files without the ignored one. Push-mirror behaviour was NOT tested on the real instance (no mirror target set up); verify it before relying on it. HTTPS auth works through `tea login helper` (SSH port 22 is unreachable from this machine).

## Caveats / pending
- Push-mirror exclusion verified on Forgejo 11 only.
- UI visibility not verified visually.
- Push mirrors to a local target need `migrations.ALLOW_LOCALNETWORKS`; irrelevant for real targets.
- GitHub as a direct target of `refs/sync/*` is not tested (design never pushes them there).
