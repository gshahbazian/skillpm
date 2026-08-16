---
name: Config schema and document editing
status: pending
---

# Goal

Read, validate, and surgically edit the global human-authored `spm.toml`.

# Scope

- Add strict version-1 config and skill models with `source`, optional `ref`, and nonempty `targets`.
- Reject unknown fields, duplicate/empty targets, unsupported versions, and structurally invalid TOML.
- Use a TOML document editor so add/remove operations preserve comments, ordering, and unrelated formatting.
- Support a symlinked `spm.toml`: resolve and atomically update the real file without replacing the symlink.
- Record original bytes for pre-commit external-change detection.
- Provide creation of a canonical empty version-1 config for first `add` and after the last removal.

# Acceptance criteria

- Round-trip edit tests prove comments and unrelated formatting survive add/remove mutations.
- Strict-schema and version tests cover all rejected forms.
- Symlinked-config tests verify the symlink remains intact.
- Atomic writes use a temporary sibling and do not leave partial files.
- Relative source/target strings remain stored as authored.
