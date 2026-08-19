---
name: Agent directory target shorthand
status: done
---

# Goal

Let `add` name a known agent skills directory instead of spelling out a full target path.

# Scope

- Add a repeatable `--agent <AGENT>` option to `add` accepting exactly the lowercase values `agents`, `pi`, and `claude`; reject any other spelling, including uppercase, with an error that lists the valid values.
- Map each value to a skills root: `agents` to `~/.agents/skills`, `pi` to `~/.pi/agent/skills`, `claude` to `~/.claude/skills`.
- Require at least one of `--target` or `--agent`; either may be used alone or together. `--target` keeps its current repeatable, non-delimited parsing.
- Expand each agent value to `<root>/<skill-name>` only after the skill name is known: from `SKILL.md` for a new source, and from the matching config entry for an already-configured source/ref.
- Order expanded targets after explicit `--target` paths, in flag order, then merge and deduplicate through the existing canonical-identity target dedup. Repeated agent values and agent paths equal to an explicit target deduplicate silently.
- Write the `~/`-spelled path verbatim into the `targets` array, preserving user-authored config formatting.
- Leave the config and lockfile schemas, `install`, `update`, and `remove` unchanged; adding an agent target to an already-configured skill must not resolve a new version.
- Update the README `Use` section and `add` examples to document `--agent`.

# Acceptance criteria

- Parser tests cover each agent value, repeated `--agent`, `--agent` combined with `--target`, `--agent` alone, an invalid/uppercase value, a bare `--agent` flag, and rejection when both `--target` and `--agent` are absent.
- Add-command tests prove a new skill records `~/.claude/skills/<name>` in `skillpm.toml` and links that target, that each mapped root resolves correctly, and that expansion uses the validated `SKILL.md` name rather than the source directory name.
- A test proves `--agent` on an already-configured source/ref adds only the new target, keeps the locked version, and reports the existing-skill summary.
- A test proves `--agent` and an equivalent explicit `--target` produce one target, retaining the first spelling.
- Existing `--target`-only behavior, including its parsing and error messages, is unchanged.
