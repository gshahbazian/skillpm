---
name: Add command workflow
status: done
---

# Goal

Implement transactional bootstrap and package addition while preserving user-authored config formatting.

# Scope

- Require source, one or more targets, and GitHub-only optional ref semantics.
- Bootstrap absent global directories, empty versioned config, and lock state.
- For a new source, resolve, snapshot, validate frontmatter name/targets, edit config, create the lock entry, and install transactionally.
- With existing config, require fresh complete lock state before mutation.
- For the same configured source/ref, merge and deduplicate targets while retaining the currently locked version.
- Treat an identical invocation as an idempotent install check.
- Reject same-name different-source/ref collisions and require explicit removal.
- Preserve config comments/order and prune after success.

# Acceptance criteria

- Tests cover first bootstrap, local/GitHub adds, optional refs, formatting preservation, same-source target merge, unchanged repeat, collision, stale lock, invalid skill/target, and rollback at every visible commit step.
- Repeated add never performs a selective version update.
