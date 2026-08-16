---
name: Lockfile schema and freshness validation
status: done
---

# Goal

Implement deterministic generated lock state for GitHub and local skills.

# Scope

- Add `src/lockfile.rs` with version-1 GitHub/local entry models.
- Store mirrored source/ref data, GitHub commits, and `sha256:` content hashes.
- Read strictly and distinguish missing, malformed, older, stale, newer, and valid lock state.
- Validate an exact entry set against config while ignoring target-only edits.
- Render entries in deterministic skill-name order with no timestamps.
- Atomically write the logical global lock path and retain original bytes for external-change detection.
- Allow update to regenerate missing/malformed/older/stale state, but never overwrite a newer version.

# Acceptance criteria

- Golden tests cover deterministic local, GitHub, mixed, and empty lockfiles.
- Freshness tests cover missing, extra, mismatched source/ref, target-only edits, and all version cases.
- Install/add/remove-facing validation rejects every non-fresh state.
- Atomic writes never expose partial TOML.
