---
name: GitHub archive snapshot preparation
status: done
---

# Goal

Fetch exact commits efficiently and convert selected GitHub paths into canonical snapshots.

# Scope

- Group requested skills by repository/ref and resolve each group once.
- Skip fetching when commits and verified snapshots are unchanged.
- Perform shallow blob-filtered fetches for changed/missing commits, with normal depth-1 fallback.
- Extract exact selected paths with local `git archive`; never copy a working tree.
- Safely unpack archives, strip the selected source prefix, and reject traversal.
- Detect and reject submodules and Git LFS pointer files.
- Validate each skill and commit its snapshot independently after the whole preparation phase succeeds.
- Bound independent preparation work to four concurrent jobs with deterministic buffered reporting.

# Acceptance criteria

- Integration tests cover repository-root and nested skills, multiple paths from one repo, unchanged fast path, fallback fetch, exact locked reconstruction, archive traversal defense, submodules, and LFS.
- Global checkout filters and line-ending settings do not alter snapshot bytes.
- A preparation failure prevents visible metadata or target changes.
