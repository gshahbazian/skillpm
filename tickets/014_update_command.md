---
name: Update command workflow
status: pending
---

# Goal

Implement the sole workflow that advances all versions, regenerates lock state, and installs the result.

# Scope

- Acquire the operation lock and load a supported config even when lock state is missing/malformed/older/stale.
- Refuse newer lock versions.
- Resolve all GitHub refs and prepare all local sources using the bounded fast paths.
- Reuse unchanged snapshots and generate one complete deterministic replacement lockfile.
- Stage installation of every skill into every target.
- Transactionally commit metadata and symlink changes with ordinary-failure rollback.
- Avoid rewriting unchanged lockfiles/links and prune unreferenced snapshots after success.

# Acceptance criteria

- Integration tests cover moved branches/tags, fixed commits, local changes, mixed unchanged sources, lock regeneration, newer-lock refusal, all-or-nothing preparation/commit failures, target repointing, idempotence, and prune warnings.
- A successful update leaves lock, store, and all targets on one consistent version set.
