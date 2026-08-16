---
name: Global paths and operation lock
status: pending
---

# Goal

Implement the single global runtime layout and concurrent-command exclusion.

# Scope

- Add `src/paths.rs` for config, lockfile, data root, snapshot store, and operation-lock paths.
- Honor `XDG_CONFIG_HOME` and `XDG_DATA_HOME` with the documented home-directory defaults.
- Provide home-relative, absolute, and leading-`~/` path resolution without shell expansion.
- Normalize paths and canonicalize existing parent components for conflict checks.
- Implement a nonblocking exclusive lock at the data-root operation-lock path.
- Create owned runtime directories with safe permissions when bootstrapping.

# Acceptance criteria

- Unit tests cover default and XDG layouts, `~/`, home-relative and absolute paths, invalid expansion syntax, and symlinked parents.
- A second process cannot acquire the operation lock and receives a clear error.
- Releasing the guard releases the lock.
- No project-directory config lookup or config override is introduced.
