# SkillPM

A skill package manager

## Install

```bash
cargo install --path .
```

Or build without installing:

```bash
cargo build --release
```

## Use

```bash
skillpm add <source> --target <path>... [--ref <ref>]
skillpm install
skillpm update
skillpm remove <name>
```

`source` is a GitHub skill or a local path:

```text
github:<owner>/<repo>/<optional-skill-path>
~/code/my-skill
```

`--target` is the install path. Its last component must match the skill name from `SKILL.md`.

```bash
skillpm add github:humanlayer/skills/plugins/show-me/skills/show-me \
  --target ~/.agents/skills/show-me

skillpm add github:schpet/linear-cli/skills/linear-cli \
  --target ~/.pi/agent/skills/linear-cli

skillpm add github:herdrdev/herdr/skills/herdr \
  --target ~/.agents/skills/herdr
```

`--ref` is optional and GitHub-only: a branch, tag, or full commit SHA. Omit it to lock the remote default branch.

```bash
skillpm add github:owner/repo/skills/foo --target ~/.agents/skills/foo --ref main
```

`install` reproduces the lockfile without resolving new versions. `update` re-resolves every source and refreshes targets. `remove` unlinks a skill's targets and drops it from the config.

Skills are declared in `~/.config/skillpm/skillpm.toml` and pinned in `~/.config/skillpm/skillpm.lock`. Snapshots live in `~/.local/share/skillpm/store`. Each target is a symlink into that store.
