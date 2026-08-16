---
name: Source parsing and skill validation
status: done
---

# Goal

Parse the two supported source forms and validate skill identity metadata.

# Scope

- Add typed GitHub and local source representations in `src/source.rs`.
- Accept only canonical `github:<owner>/<repo>/<optional-path>` GitHub strings; reject URLs, shorthand, traversal, fragments, queries, and empty components.
- Reject `ref` on local sources.
- Add `src/skill.rs` to require a regular UTF-8 `SKILL.md` with valid YAML frontmatter.
- Validate Agent Skills `name` and `description` constraints while permitting other fields.
- Validate config key and target basename equality with the frontmatter name.

# Acceptance criteria

- Table-driven tests cover valid repository-root, nested GitHub, absolute local, home-relative local, and malformed sources.
- Validation tests cover nonregular/missing/non-UTF-8 `SKILL.md`, malformed YAML, invalid names, and invalid descriptions.
- Original `SKILL.md` bytes are not rewritten.
- No alias or directory-name inference path exists.
