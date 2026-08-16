---
name: Content-addressed snapshot store
status: done
---

# Goal

Manage immutable snapshots under the XDG data root.

# Scope

- Add `src/store.rs` for staging and atomically committing `store/sha256/<hash>` snapshots.
- Deduplicate snapshots that have the same validated content hash.
- Make committed directories/files read-only while preserving executable files.
- Recompute and verify referenced hashes before installation.
- Detect corrupt/missing snapshots and expose reconstruction requirements to commands.
- Prune every snapshot not referenced by the committed lockfile.
- Treat pruning failures as warnings and clean abandoned staging data.

# Acceptance criteria

- Store tests cover commit, deduplication, integrity failure, read-only modes, staging cleanup, and pruning.
- Store traversal never follows attacker-created symlinks.
- Corruption is reported rather than trusted based only on the directory name.
- Pruning cannot remove referenced snapshots or data outside the owned store.
