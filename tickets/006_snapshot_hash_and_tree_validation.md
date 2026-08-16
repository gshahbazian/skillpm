---
name: Canonical snapshot hashing and tree validation
status: done
---

# Goal

Define one deterministic snapshot format for every source type.

# Scope

- Add `src/snapshot.rs` with the versioned, domain-separated SHA-256 encoding from README.
- Include directories (including empty), file paths/bytes/executable bits, and symlink paths/destinations in bytewise UTF-8 path order.
- Reject non-UTF-8 paths/destinations, special files, absolute/escaping links, loops, and unsupported entry types.
- Never follow symlinks during traversal.
- Treat hard links as independent regular files.
- Exclude any `.git` entry when the caller requests local-source filtering.
- Materialize a validated staged tree with exactly the entries represented by the hash.

# Acceptance criteria

- Golden hash vectors cover empty directories, executable changes, renames, symlinks, and content changes.
- Hashes ignore timestamps, ownership, and non-executable permission changes.
- Adversarial tests cover escaping/looping links, FIFOs, non-UTF-8 names, and `.git` entries.
- Hashing and materialization cannot disagree about included entries.
