---
name: Local source snapshot preparation
status: done
---

# Goal

Resolve local directories into stable locked snapshots.

# Scope

- Resolve configured local paths through the global path layer and require existing directories.
- Validate the source skill and create a filtered snapshot through the canonical snapshot layer.
- Hash the source again after staging and abort if it changed during preparation.
- Reuse the content store when the hash is unchanged.
- Implement strict reconstruction: a missing locked snapshot can be rebuilt only when the current source matches the locked hash.
- Return prepared metadata without writing config, lock, or targets.

# Acceptance criteria

- Tests cover unchanged reuse, changed content, source mutation during staging, missing sources, symlinked source parents, and exact locked reconstruction.
- `.git` entries are omitted while other hidden files and `node_modules` remain.
- Preparing a source never changes installed targets or metadata.
