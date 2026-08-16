---
name: Atomic transaction primitives
status: pending
---

# Goal

Provide reusable in-process staging, commit, and rollback without a persistent journal.

# Scope

- Add `src/transaction.rs` for temporary-sibling writes, backups, atomic renames, and rollback actions.
- Support coordinated config, lockfile, snapshot, and symlink commits.
- Preserve original bytes/links so ordinary runtime failures can roll back completed steps.
- Re-read config/lock bytes immediately before commit and abort after ordinary external edits.
- Track parent directories created by a transaction and remove only those still empty during rollback.
- Clean temporary artifacts after success and failure.
- Explicitly retain the cooperative same-user race boundary documented in README.

# Acceptance criteria

- Fault-injection tests fail each commit step and verify restoration of prior visible state.
- Interrupted file writes never expose partial metadata.
- Rollback never removes preexisting or nonempty parent directories.
- No crash journal or background recovery mechanism is introduced.
