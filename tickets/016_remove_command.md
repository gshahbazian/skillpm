---
name: Remove command workflow
status: done
---

# Goal

Remove one configured skill without deleting user data or disturbing unrelated configuration.

# Scope

- Acquire the operation lock and require supported config plus fresh complete lock state.
- Error for an unknown skill name.
- Preflight all of the skill's targets before unlinking any.
- Accept missing targets, unlink symlinks only, and abort on regular files/directories.
- Remove only the relevant config and lock entries while preserving config formatting.
- Leave valid empty files after removing the last skill.
- Roll back links and metadata on ordinary failures.
- Prune the now-unreferenced snapshot after successful commit.

# Acceptance criteria

- Tests cover present/dangling/missing links, regular target protection, unknown names, shared snapshot retention, final-skill empty state, formatting preservation, rollback, and prune warnings.
- No source, symlink destination, target parent, or regular target directory is deleted.
