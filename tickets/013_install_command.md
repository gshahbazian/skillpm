---
name: Install command workflow
status: pending
---

# Goal

Implement reproducible whole-config installation from authoritative lock state.

# Scope

- Acquire the operation lock and load supported config plus fresh exact lock state.
- Validate the complete source/target graph before committing.
- Verify every referenced snapshot.
- Reconstruct missing/corrupt GitHub snapshots at exact locked commits and local snapshots only on exact source-hash match.
- Plan and transactionally reconcile every target symlink.
- Avoid metadata writes, network access with a populated valid store, and replacement of correct links.
- Emit deterministic noninteractive progress/errors and a concise summary.

# Acceptance criteria

- Integration tests cover offline populated-store install, exact GitHub cache reconstruction, local reconstruction success/failure, stale lock rejection, corrupt store repair, idempotence, and multi-target rollback.
- `install` never changes config, lock versions, or source directories.
