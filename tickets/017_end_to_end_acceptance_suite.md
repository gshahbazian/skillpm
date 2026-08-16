---
name: End-to-end acceptance suite
status: done
---

# Goal

Verify the complete README contract as a released CLI across real process and filesystem boundaries.

# Scope

- Build black-box fixtures for isolated HOME/XDG roots, local GitHub-style remotes, local skills, symlinked configs/parents, and multiple targets.
- Exercise complete `add -> install -> update -> remove` lifecycles.
- Verify deterministic lock and snapshot output across repeated runs.
- Verify global-only lookup, process-lock contention, timeout cleanup, `NO_COLOR`, stdout/stderr separation, and unsupported-platform handling where testable.
- Add fault-injection coverage for transactional rollback across multiple skills and targets.
- Audit help text and README examples against the implemented CLI.
- Remove obsolete placeholder code and ensure formatting, linting, and the complete test suite pass.

# Acceptance criteria

- Every completion criterion in README section 12 has an explicit automated test or documented platform-specific justification.
- Tests prove target regular files/directories are never replaced or recursively deleted.
- The repository builds cleanly and all tests pass from a fresh checkout.
- No command or option outside the documented v1 surface remains.
