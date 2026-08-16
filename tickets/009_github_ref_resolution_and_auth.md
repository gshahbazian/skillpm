---
name: GitHub ref resolution and authentication
status: done
---

# Goal

Resolve canonical GitHub sources to exact commits through noninteractive system Git.

# Scope

- Add the Git process runner in `src/github.rs` with buffered/redacted output and temporary cleanup.
- Require system Git, set `GIT_TERMINAL_PROMPT=0`, and enforce the five-minute configurable timeout.
- Resolve default HEAD, branches, lightweight/annotated tags, and full commit SHAs.
- Reject branch/tag ambiguity and malformed resolved object IDs.
- Let ordinary credential helpers run; on GitHub auth failure retry with `GITHUB_TOKEN`, then `GH_TOKEN`, without exposing credentials in arguments, URLs, logs, or errors.
- Do not invoke `gh` or implement SSH fallback.

# Acceptance criteria

- Tests use controlled local remotes/fake Git to cover every ref type, moved refs, ambiguity, timeout, missing Git, auth fallback order, and redaction.
- Full commit refs remain fixed and install-facing resolution never advances a lock.
- Temporary resources and child processes are cleaned up after errors/timeouts.
