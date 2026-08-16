#![allow(dead_code)] // consumed by the store and command tickets

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Version and domain prefix of the canonical hash input. Bump only with a
/// snapshot format change; every stored hash depends on it.
const HASH_DOMAIN: &[u8] = b"spm snapshot v1\n";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GitFilter {
  /// Local sources: skip any entry named `.git` at any depth.
  ExcludeGit,
  /// Git-archive extractions cannot contain `.git`; nothing is skipped.
  IncludeAll,
}

/// A validated source tree: the exact entry set that hashing and
/// materialization both consume, so they cannot disagree. Fields are private
/// so validated entries cannot be altered after the scan.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotTree {
  root: PathBuf,
  /// Sorted bytewise by relative UTF-8 path.
  entries: Vec<SnapshotEntry>,
}

impl SnapshotTree {
  pub fn entries(&self) -> &[SnapshotEntry] {
    &self.entries
  }

  pub fn root(&self) -> &Path {
    &self.root
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEntry {
  /// Normalized `/`-separated path relative to the tree root.
  pub path: String,
  pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
  Dir,
  File { executable: bool },
  Symlink { destination: String },
}

/// Walks `root` without ever following symlinks, validating every entry.
pub fn scan_tree(root: &Path, filter: GitFilter) -> Result<SnapshotTree> {
  let metadata = fs::symlink_metadata(root)
    .with_context(|| format!("failed to read snapshot root {}", root.display()))?;
  if !metadata.is_dir() {
    bail!("snapshot root {} must be a directory", root.display());
  }

  let mut entries = Vec::new();
  walk("", root, filter, &mut entries)?;
  entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

  validate_symlink_graph(&entries)?;

  Ok(SnapshotTree {
    root: root.to_path_buf(),
    entries,
  })
}

fn walk(rel: &str, dir: &Path, filter: GitFilter, entries: &mut Vec<SnapshotEntry>) -> Result<()> {
  let listing =
    fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?;

  for entry in listing {
    let entry = entry.with_context(|| format!("failed to read directory {}", dir.display()))?;

    let name = entry.file_name();
    let Some(name) = name.to_str() else {
      bail!(
        "non-UTF-8 path {:?} in {} is not supported",
        entry.file_name(),
        dir.display()
      );
    };
    if filter == GitFilter::ExcludeGit && name == ".git" {
      continue;
    }

    let path = if rel.is_empty() {
      name.to_string()
    } else {
      format!("{rel}/{name}")
    };
    let absolute = entry.path();
    let file_type = fs::symlink_metadata(&absolute)
      .with_context(|| format!("failed to stat {}", absolute.display()))?
      .file_type();

    if file_type.is_symlink() {
      let destination = fs::read_link(&absolute)
        .with_context(|| format!("failed to read symlink {}", absolute.display()))?;
      let Some(destination) = destination.to_str() else {
        bail!(
          "symlink {} has a non-UTF-8 destination, which is not supported",
          absolute.display()
        );
      };
      if destination.starts_with('/') {
        bail!("symlink '{path}' has an absolute destination '{destination}'");
      }
      entries.push(SnapshotEntry {
        path,
        kind: EntryKind::Symlink {
          destination: destination.to_string(),
        },
      });
    } else if file_type.is_dir() {
      entries.push(SnapshotEntry {
        path: path.clone(),
        kind: EntryKind::Dir,
      });
      walk(&path, &absolute, filter, entries)?;
    } else if file_type.is_file() {
      entries.push(SnapshotEntry {
        path,
        kind: EntryKind::File {
          executable: is_executable(&absolute)?,
        },
      });
    } else {
      bail!(
        "{} is not a regular file, directory, or symlink; sockets, devices, and FIFOs are not supported",
        absolute.display()
      );
    }
  }

  Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool> {
  use std::os::unix::fs::MetadataExt;
  let mode = fs::symlink_metadata(path)?.mode();
  Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool> {
  Ok(false)
}

/// Resolves every symlink destination the way the kernel would — expanding
/// any path component that is itself a symlink in the tree — so a chain like
/// `d -> .` plus `leak -> d/../outside` cannot pass a purely lexical check.
/// Loops and over-deep chains exhaust the expansion budget.
fn validate_symlink_graph(entries: &[SnapshotEntry]) -> Result<()> {
  let mut links: HashMap<&str, &str> = HashMap::new();
  for entry in entries {
    if let EntryKind::Symlink { destination } = &entry.kind {
      links.insert(&entry.path, destination);
    }
  }

  for (path, destination) in &links {
    let mut base: Vec<String> = path.split('/').map(String::from).collect();
    base.pop(); // the link itself

    // 40 matches the kernel's ELOOP limit and doubles as loop detection
    let mut budget = 40usize;
    resolve_destination(&links, &mut base, destination, &mut budget, path)?;
  }

  Ok(())
}

fn resolve_destination(
  links: &HashMap<&str, &str>,
  base: &mut Vec<String>,
  relative: &str,
  budget: &mut usize,
  origin: &str,
) -> Result<()> {
  for component in relative.split('/') {
    match component {
      "" => bail!("symlink '{origin}' has invalid destination '{relative}'"),
      "." => {}
      ".." => {
        if base.pop().is_none() {
          bail!("symlink '{origin}' escapes the snapshot root");
        }
      }
      name => {
        base.push(name.to_string());
        let candidate = base.join("/");
        if let Some(target) = links.get(candidate.as_str()) {
          if *budget == 0 {
            bail!("symlink loop or over-deep chain involving '{origin}'");
          }
          *budget -= 1;
          base.pop(); // the link component is replaced by its expansion
          resolve_destination(links, base, target, budget, origin)?;
        }
      }
    }
  }

  Ok(())
}

/// The versioned, domain-separated canonical encoding. Length-prefixed fields
/// under one-byte entry tags; file bytes are read at hash time.
pub fn hash_tree(tree: &SnapshotTree) -> Result<String> {
  let mut hasher = Sha256::new();
  hasher.update(HASH_DOMAIN);

  for entry in &tree.entries {
    match &entry.kind {
      EntryKind::Dir => {
        hasher.update(b"D");
        update_bytes(&mut hasher, entry.path.as_bytes());
      }
      EntryKind::File { executable } => {
        hasher.update(b"F");
        update_bytes(&mut hasher, entry.path.as_bytes());
        hasher.update([u8::from(*executable)]);
        let source = tree.root.join(&entry.path);
        let contents =
          fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?;
        update_bytes(&mut hasher, &contents);
      }
      EntryKind::Symlink { destination } => {
        hasher.update(b"L");
        update_bytes(&mut hasher, entry.path.as_bytes());
        update_bytes(&mut hasher, destination.as_bytes());
      }
    }
  }

  let digest = hasher.finalize();
  let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
  Ok(format!("sha256:{hex}"))
}

fn update_bytes(hasher: &mut Sha256, bytes: &[u8]) {
  hasher.update((bytes.len() as u64).to_be_bytes());
  hasher.update(bytes);
}

/// Creates `destination` (which must not exist) with exactly the tree's
/// entries. Permissions are normalized: 0755 dirs/executables, 0644 files.
pub fn materialize_tree(tree: &SnapshotTree, destination: &Path) -> Result<()> {
  fs::create_dir(destination)
    .with_context(|| format!("failed to create staging dir {}", destination.display()))?;

  for entry in &tree.entries {
    let target = destination.join(&entry.path);

    match &entry.kind {
      EntryKind::Dir => {
        fs::create_dir(&target)
          .with_context(|| format!("failed to create {}", target.display()))?;
      }
      EntryKind::File { executable } => {
        let source = tree.root.join(&entry.path);
        fs::copy(&source, &target)
          .with_context(|| format!("failed to copy {}", source.display()))?;
        set_file_mode(&target, *executable)?;
      }
      EntryKind::Symlink { destination: dest } => {
        make_symlink(dest, &target)?;
      }
    }
  }

  Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path, executable: bool) -> Result<()> {
  use std::os::unix::fs::PermissionsExt;
  let mode = if executable { 0o755 } else { 0o644 };
  fs::set_permissions(path, fs::Permissions::from_mode(mode))
    .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _executable: bool) -> Result<()> {
  Ok(())
}

#[cfg(unix)]
fn make_symlink(destination: &str, link: &Path) -> Result<()> {
  std::os::unix::fs::symlink(destination, link)
    .with_context(|| format!("failed to create symlink {}", link.display()))
}

#[cfg(not(unix))]
fn make_symlink(_destination: &str, link: &Path) -> Result<()> {
  bail!("cannot create symlink {} on this platform", link.display())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scan(root: &Path) -> SnapshotTree {
    scan_tree(root, GitFilter::ExcludeGit).unwrap()
  }

  fn hash_of(root: &Path) -> String {
    hash_tree(&scan(root)).unwrap()
  }

  #[cfg(unix)]
  fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
  }

  /// The fixture behind the golden vectors: file, executable, empty dir,
  /// nested file, and an internal symlink.
  fn build_fixture(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::write(root.join("SKILL.md"), "---\nname: x\ndescription: y\n---\n").unwrap();
    fs::write(root.join("docs/guide.md"), "guide\n").unwrap();
    fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
      make_executable(&root.join("run.sh"));
      std::os::unix::fs::symlink("docs/guide.md", root.join("link")).unwrap();
    }
  }

  const GOLDEN_EMPTY: &str =
    "sha256:3fef2482d0025e9aad92e3359b9d950196e42373f47933c60f6ff2baf032e438";
  #[cfg(unix)]
  const GOLDEN_FIXTURE: &str =
    "sha256:abcf59c152b671d51a2de23100f7f6f6d6fd2be8455ec928e15c635b606962f3";

  #[test]
  fn golden_empty_tree() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(hash_of(temp.path()), GOLDEN_EMPTY);
  }

  #[test]
  #[cfg(unix)]
  fn golden_fixture_tree() {
    let temp = tempfile::tempdir().unwrap();
    build_fixture(temp.path());
    assert_eq!(hash_of(temp.path()), GOLDEN_FIXTURE);
  }

  #[test]
  #[cfg(unix)]
  fn hash_changes_on_meaningful_edits() {
    let cases: &[fn(&Path)] = &[
      // content change
      |root| fs::write(root.join("docs/guide.md"), "changed\n").unwrap(),
      // rename
      |root| fs::rename(root.join("docs/guide.md"), root.join("docs/renamed.md")).unwrap(),
      // executable bit flipped off
      |root| {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
      },
      // symlink destination change
      |root| {
        fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink("SKILL.md", root.join("link")).unwrap();
      },
      // new empty directory
      |root| fs::create_dir(root.join("empty2")).unwrap(),
    ];

    for (index, edit) in cases.iter().enumerate() {
      let temp = tempfile::tempdir().unwrap();
      build_fixture(temp.path());
      edit(temp.path());
      assert_ne!(
        hash_of(temp.path()),
        GOLDEN_FIXTURE,
        "edit {index} must change the hash"
      );
    }
  }

  #[test]
  #[cfg(unix)]
  fn hash_ignores_timestamps_and_non_executable_permission_bits() {
    use std::fs::FileTimes;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    let temp = tempfile::tempdir().unwrap();
    build_fixture(temp.path());

    let file = fs::File::options()
      .append(true)
      .open(temp.path().join("docs/guide.md"))
      .unwrap();
    file
      .set_times(
        FileTimes::new()
          .set_accessed(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
          .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
      )
      .unwrap();

    // 0600 vs 0644: no 0o111 bit involved, so the hash must not move
    fs::set_permissions(
      temp.path().join("docs/guide.md"),
      fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    assert_eq!(hash_of(temp.path()), GOLDEN_FIXTURE);
  }

  #[test]
  #[cfg(unix)]
  fn hard_links_hash_as_independent_files() {
    let a = tempfile::tempdir().unwrap();
    fs::write(a.path().join("one"), "same\n").unwrap();
    fs::hard_link(a.path().join("one"), a.path().join("two")).unwrap();

    let b = tempfile::tempdir().unwrap();
    fs::write(b.path().join("one"), "same\n").unwrap();
    fs::write(b.path().join("two"), "same\n").unwrap();

    assert_eq!(hash_of(a.path()), hash_of(b.path()));
  }

  #[test]
  fn git_entries_are_excluded_only_for_local_filtering() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("keep.md"), "x").unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".git/config"), "secret").unwrap();
    fs::create_dir(temp.path().join("nested")).unwrap();
    fs::write(temp.path().join("nested/.git"), "gitfile").unwrap();

    let filtered = scan_tree(temp.path(), GitFilter::ExcludeGit).unwrap();
    let paths: Vec<&str> = filtered.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["keep.md", "nested"]);

    let unfiltered = scan_tree(temp.path(), GitFilter::IncludeAll).unwrap();
    assert!(unfiltered.entries.iter().any(|e| e.path == ".git/config"));
    assert!(unfiltered.entries.iter().any(|e| e.path == "nested/.git"));
  }

  #[test]
  #[cfg(unix)]
  fn adversarial_trees_are_rejected() {
    type Setup = fn(&Path);
    let cases: &[(&str, Setup, &str)] = &[
      (
        "absolute symlink",
        |root| std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap(),
        "absolute destination",
      ),
      (
        "escaping symlink",
        |root| std::os::unix::fs::symlink("../outside", root.join("link")).unwrap(),
        "escapes the snapshot root",
      ),
      (
        "escaping via intermediate dotdot",
        |root| {
          fs::create_dir(root.join("sub")).unwrap();
          std::os::unix::fs::symlink("../../outside", root.join("sub/link")).unwrap();
        },
        "escapes the snapshot root",
      ),
      (
        "two-link loop",
        |root| {
          std::os::unix::fs::symlink("b", root.join("a")).unwrap();
          std::os::unix::fs::symlink("a", root.join("b")).unwrap();
        },
        "symlink loop",
      ),
      (
        "self loop",
        |root| std::os::unix::fs::symlink("a", root.join("a")).unwrap(),
        "symlink loop",
      ),
      (
        // lexically "outside" stays inside, but resolving d first makes
        // d/.. the root's parent
        "escape through a directory symlink",
        |root| {
          std::os::unix::fs::symlink(".", root.join("d")).unwrap();
          std::os::unix::fs::symlink("d/../outside", root.join("leak")).unwrap();
        },
        "escapes the snapshot root",
      ),
      (
        "loop through a directory symlink",
        |root| {
          std::os::unix::fs::symlink(".", root.join("d")).unwrap();
          std::os::unix::fs::symlink("d/a", root.join("a")).unwrap();
        },
        "symlink loop",
      ),
      (
        "fifo",
        |root| {
          let status = std::process::Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .unwrap();
          assert!(status.success());
        },
        "FIFOs are not supported",
      ),
    ];

    for (label, setup, expected) in cases {
      let temp = tempfile::tempdir().unwrap();
      setup(temp.path());
      let error = scan_tree(temp.path(), GitFilter::ExcludeGit).unwrap_err();
      assert!(
        format!("{error:#}").contains(expected),
        "{label}: expected '{expected}', got: {error:#}"
      );
    }
  }

  #[test]
  #[cfg(unix)]
  fn non_utf8_names_and_destinations_are_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // APFS refuses to even create non-UTF-8 names (EILSEQ); the scan check
    // matters on filesystems like ext4 that allow arbitrary bytes
    let temp = tempfile::tempdir().unwrap();
    if fs::write(temp.path().join(OsStr::from_bytes(b"bad\xff")), "x").is_ok() {
      let error = scan_tree(temp.path(), GitFilter::ExcludeGit).unwrap_err();
      assert!(error.to_string().contains("non-UTF-8 path"));
    }

    let temp = tempfile::tempdir().unwrap();
    if std::os::unix::fs::symlink(OsStr::from_bytes(b"bad\xff"), temp.path().join("link")).is_ok() {
      let error = scan_tree(temp.path(), GitFilter::ExcludeGit).unwrap_err();
      assert!(error.to_string().contains("non-UTF-8 destination"));
    }
  }

  #[test]
  #[cfg(unix)]
  fn benign_directory_symlink_chains_are_allowed() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("sub")).unwrap();
    fs::write(temp.path().join("sub/file"), "x").unwrap();
    std::os::unix::fs::symlink("sub", temp.path().join("d")).unwrap();
    std::os::unix::fs::symlink("d/file", temp.path().join("x")).unwrap();
    std::os::unix::fs::symlink("sub/../sub/file", temp.path().join("y")).unwrap();

    scan(temp.path());
  }

  #[test]
  #[cfg(unix)]
  fn dangling_internal_symlinks_are_allowed() {
    let temp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("missing-yet", temp.path().join("link")).unwrap();

    let tree = scan(temp.path());
    assert_eq!(tree.entries.len(), 1);
  }

  #[test]
  #[cfg(unix)]
  fn materialization_round_trips_the_hash_and_entries() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    build_fixture(&source);

    let tree = scan(&source);
    let staged = temp.path().join("staged");
    materialize_tree(&tree, &staged).unwrap();

    let restaged = scan(&staged);
    assert_eq!(restaged.entries, tree.entries);
    assert_eq!(hash_tree(&restaged).unwrap(), GOLDEN_FIXTURE);
  }

  #[test]
  fn materialization_requires_a_fresh_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();

    let tree = scan(&source);
    let error = materialize_tree(&tree, temp.path()).unwrap_err();
    assert!(format!("{error:#}").contains("failed to create staging dir"));
  }
}
