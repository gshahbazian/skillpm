---
name: Target planning and symlink transactions
status: done
---

# Goal

Validate the complete target graph and safely reconcile targets as absolute symlinks.

# Scope

- Add `src/targets.rs` to resolve targets from home and canonicalize existing parents.
- Reject normalized duplicates, cross-skill sharing, ancestor/descendant overlap, protected-path overlap, local-source overlap, and basename/name mismatch.
- Preflight every target before mutation.
- Create missing parent directories transactionally.
- Create absolute symlinks, no-op correct links, and atomically replace other/dangling symlinks without touching destinations.
- Reject regular files/directories and provide no force path.
- Implement removal planning: missing is acceptable, symlink is unlinkable, regular file/directory aborts all.

# Acceptance criteria

- Unit/integration tests cover every conflict class, symlinked parent aliases, dangling/correct/wrong links, and regular target protection.
- Multi-target fault injection rolls back earlier link changes.
- No target operation recursively removes a directory or follows a destination link.
