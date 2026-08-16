---
name: CLI contract and platform guard
status: pending
---

# Goal

Establish the final noninteractive CLI surface without implementing command workflows.

# Scope

- Move Clap definitions into `src/cli.rs`.
- Parse exactly `install`, `update`, `add <source> --target <path>... [--ref <ref>]`, and `remove <name>`.
- Require at least one target for `add` and reject unsupported flags.
- Add the macOS/Linux runtime guard and a clear Windows error.
- Add shared output helpers: progress/diagnostics on stderr, success summaries on stdout, TTY-aware color, and `NO_COLOR` support.
- Keep command dispatch thin so later tickets can attach implementations.

# Acceptance criteria

- Parser tests cover every valid command and representative invalid combinations.
- `add --ref` is parsed but source-specific validation remains delegated to the source layer.
- No config override, alias, force, selective update/install, JSON, quiet, or prompt options exist.
- Existing placeholder command logic is removed or isolated behind the command module.
