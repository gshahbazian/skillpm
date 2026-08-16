#![allow(dead_code)] // consumed by the command tickets

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::create_private_dir;
use crate::snapshot::{self, EntryKind, GitFilter, SnapshotTree};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SnapshotStatus {
  Missing,
  Valid,
  /// Present but failing verification; the caller must delete and
  /// reconstruct it from the locked source.
  Corrupt,
}

#[derive(Debug)]
pub struct Store {
  root: PathBuf,
}

impl Store {
  pub fn new(store_root: &Path) -> Self {
    Self {
      root: store_root.to_path_buf(),
    }
  }

  fn sha256_dir(&self) -> PathBuf {
    self.root.join("sha256")
  }

  fn staging_dir(&self) -> PathBuf {
    self.root.join("staging")
  }

  /// Validates the hash format before joining, so a hostile hash string can
  /// never traverse outside the store.
  pub fn snapshot_path(&self, content_hash: &str) -> Result<PathBuf> {
    Ok(self.sha256_dir().join(parse_hash(content_hash)?))
  }

  /// symlink_metadata only inspects the leaf, so the root must be checked
  /// separately before any child path is trusted.
  fn structural_state(&self, child: &Path) -> Result<DirState> {
    if structural_dir_state(&self.root)? == DirState::Missing {
      return Ok(DirState::Missing);
    }
    structural_dir_state(child)
  }

  /// Recomputes the snapshot's hash; the directory name alone is never
  /// trusted. A symlink squatting at the snapshot path is corrupt, not
  /// followed. Writable permission drift is repaired before reporting Valid.
  pub fn verify_snapshot(&self, content_hash: &str) -> Result<SnapshotStatus> {
    let path = self.snapshot_path(content_hash)?;

    if self.structural_state(&self.sha256_dir())? == DirState::Missing {
      return Ok(SnapshotStatus::Missing);
    }

    match fs::symlink_metadata(&path) {
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SnapshotStatus::Missing),
      Err(error) => {
        return Err(error).with_context(|| format!("failed to stat {}", path.display()));
      }
      Ok(metadata) if !metadata.is_dir() => return Ok(SnapshotStatus::Corrupt),
      Ok(_) => {}
    }

    let Ok(tree) = snapshot::scan_tree(&path, GitFilter::IncludeAll) else {
      return Ok(SnapshotStatus::Corrupt);
    };
    let Ok(actual) = snapshot::hash_tree(&tree) else {
      return Ok(SnapshotStatus::Corrupt);
    };
    if actual != content_hash {
      return Ok(SnapshotStatus::Corrupt);
    }

    // a hash match with drifted modes is repairable in place; a snapshot we
    // cannot make read-only again is not trustworthy
    if apply_read_only(&path, &tree).is_err() {
      return Ok(SnapshotStatus::Corrupt);
    }

    Ok(SnapshotStatus::Valid)
  }

  /// Stages, re-scans, and atomically commits a snapshot of `source_tree`.
  /// The returned content hash is computed from the staged bytes — what the
  /// store actually holds — never from the source scan.
  pub fn commit_tree(&self, source_tree: &SnapshotTree) -> Result<String> {
    for dir in [&self.root, &self.sha256_dir(), &self.staging_dir()] {
      structural_dir_state(dir)?;
      create_private_dir(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }

    // TempDir cleans up staging on any failure path
    let staging = tempfile::tempdir_in(self.staging_dir()).with_context(|| {
      format!(
        "failed to create staging in {}",
        self.staging_dir().display()
      )
    })?;
    let staged_root = staging.path().join("snapshot");
    snapshot::materialize_tree(source_tree, &staged_root)?;

    let staged_tree = snapshot::scan_tree(&staged_root, GitFilter::IncludeAll)
      .context("failed to re-scan staged snapshot")?;
    if staged_tree.entries() != source_tree.entries() {
      bail!("staged snapshot does not match its source; the source changed during staging");
    }
    let content_hash = snapshot::hash_tree(&staged_tree)?;

    let destination = self.snapshot_path(&content_hash)?;
    match self.verify_snapshot(&content_hash)? {
      // deduplicate against an existing valid snapshot
      SnapshotStatus::Valid => return Ok(content_hash),
      SnapshotStatus::Corrupt => {
        remove_tree(&destination)
          .with_context(|| format!("failed to remove corrupt {}", destination.display()))?;
      }
      SnapshotStatus::Missing => {}
    }

    // rename before chmod: macOS refuses to move a read-only directory
    // (the rename must rewrite its '..' entry)
    fs::rename(&staged_root, &destination).with_context(|| {
      format!(
        "failed to commit snapshot {} into the store",
        destination.display()
      )
    })?;

    // a snapshot that cannot be made read-only must not stay in the store
    if let Err(error) = apply_read_only(&destination, &staged_tree) {
      let _ = remove_tree(&destination);
      return Err(error);
    }

    Ok(content_hash)
  }

  /// For reconstruction of corrupt snapshots; missing is fine.
  pub fn remove_snapshot(&self, content_hash: &str) -> Result<()> {
    let path = self.snapshot_path(content_hash)?;
    if self.structural_state(&self.sha256_dir())? == DirState::Missing {
      return Ok(());
    }
    match fs::symlink_metadata(&path) {
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
      Ok(_) => remove_tree(&path).with_context(|| format!("failed to remove {}", path.display())),
    }
  }

  /// Deletes every snapshot not in `referenced` and all abandoned staging
  /// data. Failures come back as warnings: pruning never fails a command.
  pub fn prune(&self, referenced: &BTreeSet<String>) -> Vec<String> {
    let mut warnings = Vec::new();

    // a symlinked root would redirect both subdirectory traversals
    match structural_dir_state(&self.root) {
      Ok(DirState::Present) => {}
      Ok(DirState::Missing) => return warnings,
      Err(error) => {
        warnings.push(format!("{error:#}"));
        return warnings;
      }
    }

    match structural_dir_state(&self.staging_dir()) {
      Ok(DirState::Present) => prune_dir(&self.staging_dir(), &mut warnings, |_| true),
      Ok(DirState::Missing) => {}
      Err(error) => warnings.push(format!("{error:#}")),
    }

    match structural_dir_state(&self.sha256_dir()) {
      Ok(DirState::Present) => prune_dir(&self.sha256_dir(), &mut warnings, |name| {
        if !is_lower_hex64(name) {
          // not something spm wrote; leave it alone
          return false;
        }
        !referenced.contains(&format!("sha256:{name}"))
      }),
      Ok(DirState::Missing) => {}
      Err(error) => warnings.push(format!("{error:#}")),
    }

    warnings
  }
}

#[derive(Debug, PartialEq)]
enum DirState {
  Missing,
  Present,
}

/// The store's structural directories must be real directories. A symlink
/// here would redirect every traversal — including pruning's deletions —
/// outside the store.
fn structural_dir_state(path: &Path) -> Result<DirState> {
  match fs::symlink_metadata(path) {
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DirState::Missing),
    Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    Ok(metadata) if metadata.file_type().is_symlink() => {
      bail!("{} is a symlink; refusing to traverse it", path.display())
    }
    Ok(metadata) if !metadata.is_dir() => {
      bail!("{} is not a directory", path.display())
    }
    Ok(_) => Ok(DirState::Present),
  }
}

/// Removes entries of `dir` for which `should_remove(name)` is true.
fn prune_dir(dir: &Path, warnings: &mut Vec<String>, should_remove: impl Fn(&str) -> bool) {
  let listing = match fs::read_dir(dir) {
    Ok(listing) => listing,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return,
    Err(error) => {
      warnings.push(format!("failed to read {}: {error}", dir.display()));
      return;
    }
  };

  for entry in listing {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        warnings.push(format!("failed to read {}: {error}", dir.display()));
        continue;
      }
    };

    let name = entry.file_name();
    if !name.to_str().is_some_and(&should_remove) {
      continue;
    }

    if let Err(error) = remove_tree(&entry.path()) {
      warnings.push(format!(
        "failed to prune {}: {error}",
        entry.path().display()
      ));
    }
  }
}

/// remove_dir_all over possibly read-only content: directories get write
/// permission restored first. Never follows symlinks.
fn remove_tree(path: &Path) -> io::Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() {
    return fs::remove_file(path);
  }

  restore_dir_write(path)?;
  fs::remove_dir_all(path)
}

fn restore_dir_write(dir: &Path) -> io::Result<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
  }

  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    if fs::symlink_metadata(entry.path())?.is_dir() {
      restore_dir_write(&entry.path())?;
    }
  }
  Ok(())
}

/// Enforces 0555 directories, 0555 executables, 0444 files, chmodding only
/// entries that drifted. Symlinks carry no mode.
fn apply_read_only(root: &Path, tree: &SnapshotTree) -> Result<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;

    let enforce = |path: &Path, mode: u32| -> Result<()> {
      let current = fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions()
        .mode();
      if current & 0o7777 == mode {
        return Ok(());
      }
      fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
    };

    for entry in tree.entries() {
      let path = root.join(&entry.path);
      match &entry.kind {
        EntryKind::Dir => enforce(&path, 0o555)?,
        EntryKind::File { executable: true } => enforce(&path, 0o555)?,
        EntryKind::File { executable: false } => enforce(&path, 0o444)?,
        EntryKind::Symlink { .. } => {}
      }
    }
    enforce(root, 0o555)?;
  }

  Ok(())
}

fn parse_hash(content_hash: &str) -> Result<&str> {
  let hex = content_hash
    .strip_prefix("sha256:")
    .filter(|hex| is_lower_hex64(hex))
    .with_context(|| format!("invalid content hash '{content_hash}'"))?;
  Ok(hex)
}

fn is_lower_hex64(text: &str) -> bool {
  text.len() == 64
    && text
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    source: PathBuf,
  }

  fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::new(&temp.path().join("store"));

    let source = temp.path().join("source");
    fs::create_dir_all(source.join("docs")).unwrap();
    fs::write(
      source.join("SKILL.md"),
      "---\nname: x\ndescription: y\n---\n",
    )
    .unwrap();
    fs::write(source.join("docs/guide.md"), "guide\n").unwrap();
    fs::write(source.join("run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    Fixture {
      _temp: temp,
      store,
      source,
    }
  }

  fn commit(fixture: &Fixture) -> String {
    let tree = snapshot::scan_tree(&fixture.source, GitFilter::ExcludeGit).unwrap();
    fixture.store.commit_tree(&tree).unwrap()
  }

  #[cfg(unix)]
  fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
  }

  #[test]
  fn commit_stores_and_verifies() {
    let fixture = fixture();
    let hash = commit(&fixture);

    let path = fixture.store.snapshot_path(&hash).unwrap();
    assert!(path.is_dir());
    assert_eq!(
      fs::read_to_string(path.join("docs/guide.md")).unwrap(),
      "guide\n"
    );
    assert_eq!(
      fixture.store.verify_snapshot(&hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  #[cfg(unix)]
  fn committed_snapshots_are_read_only_with_executables_preserved() {
    let fixture = fixture();
    let hash = commit(&fixture);
    let path = fixture.store.snapshot_path(&hash).unwrap();

    assert_eq!(mode_of(&path), 0o555);
    assert_eq!(mode_of(&path.join("docs")), 0o555);
    assert_eq!(mode_of(&path.join("docs/guide.md")), 0o444);
    assert_eq!(mode_of(&path.join("run.sh")), 0o555);
  }

  #[test]
  fn commit_deduplicates_identical_content() {
    let fixture = fixture();
    let first = commit(&fixture);
    let second = commit(&fixture);
    assert_eq!(first, second);

    let entries = fs::read_dir(fixture.store.sha256_dir()).unwrap().count();
    assert_eq!(entries, 1);

    // staging left nothing behind
    let staged = fs::read_dir(fixture.store.staging_dir()).unwrap().count();
    assert_eq!(staged, 0);
  }

  #[test]
  #[cfg(unix)]
  fn tampered_snapshots_are_reported_corrupt_and_replaceable() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let hash = commit(&fixture);
    let path = fixture.store.snapshot_path(&hash).unwrap();

    // tamper: restore write and change content
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(path.join("docs"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(
      path.join("docs/guide.md"),
      fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::write(path.join("docs/guide.md"), "tampered\n").unwrap();

    assert_eq!(
      fixture.store.verify_snapshot(&hash).unwrap(),
      SnapshotStatus::Corrupt
    );

    // recommitting the source replaces the corrupt copy
    let recommitted = commit(&fixture);
    assert_eq!(recommitted, hash);
    assert_eq!(
      fixture.store.verify_snapshot(&hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  fn missing_snapshots_are_reported_missing() {
    let fixture = fixture();
    let absent = format!("sha256:{}", "0".repeat(64));
    assert_eq!(
      fixture.store.verify_snapshot(&absent).unwrap(),
      SnapshotStatus::Missing
    );
  }

  #[test]
  #[cfg(unix)]
  fn symlink_at_snapshot_path_is_corrupt_not_followed() {
    let fixture = fixture();
    let hash = commit(&fixture);
    let real = fixture.store.snapshot_path(&hash).unwrap();

    // squat a symlink to the valid snapshot under a different hash name
    let fake_hash = format!("sha256:{}", "f".repeat(64));
    let fake_path = fixture.store.snapshot_path(&fake_hash).unwrap();
    std::os::unix::fs::symlink(&real, &fake_path).unwrap();

    assert_eq!(
      fixture.store.verify_snapshot(&fake_hash).unwrap(),
      SnapshotStatus::Corrupt
    );
    // the squatting link also cannot make its own hash "valid"
    let link_to_self = fixture.store.snapshot_path(&hash).unwrap();
    assert!(link_to_self.is_dir());
  }

  #[test]
  fn hostile_hash_strings_cannot_traverse() {
    let fixture = fixture();
    let cases = [
      "sha256:../../../etc",
      "sha256:..",
      &format!("sha256:{}", "A".repeat(64)),
      &format!("md5:{}", "a".repeat(64)),
      "sha256:short",
      "",
    ];

    for hash in cases {
      assert!(
        fixture.store.snapshot_path(hash).is_err(),
        "hash must be rejected: {hash:?}"
      );
    }
  }

  #[test]
  fn prune_removes_unreferenced_and_staging_but_keeps_referenced() {
    let fixture = fixture();
    let keep = commit(&fixture);

    fs::write(fixture.source.join("extra.md"), "more\n").unwrap();
    let drop = commit(&fixture);
    assert_ne!(keep, drop);

    // abandoned staging data from a crashed run
    let abandoned = fixture.store.staging_dir().join("left-behind");
    fs::create_dir_all(abandoned.join("deep")).unwrap();
    fs::write(abandoned.join("deep/file"), "x").unwrap();

    // an entry spm did not write stays untouched
    let foreign = fixture.store.sha256_dir().join("not-a-hash");
    fs::create_dir(&foreign).unwrap();

    let referenced = BTreeSet::from([keep.clone()]);
    let warnings = fixture.store.prune(&referenced);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    assert_eq!(
      fixture.store.verify_snapshot(&keep).unwrap(),
      SnapshotStatus::Valid
    );
    assert_eq!(
      fixture.store.verify_snapshot(&drop).unwrap(),
      SnapshotStatus::Missing
    );
    assert!(!abandoned.exists());
    assert!(foreign.exists());
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_structural_dirs_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let victim = temp.path().join("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("precious"), "data").unwrap();

    // store/staging is a symlink pointing at the victim directory
    let store_root = temp.path().join("store");
    fs::create_dir(&store_root).unwrap();
    std::os::unix::fs::symlink(&victim, store_root.join("staging")).unwrap();

    let store = Store::new(&store_root);
    let warnings = store.prune(&BTreeSet::new());
    assert!(
      warnings.iter().any(|w| w.contains("refusing to traverse")),
      "expected a symlink warning: {warnings:?}"
    );
    assert!(
      victim.join("precious").exists(),
      "prune must not follow the link"
    );

    // commits refuse a symlinked sha256 dir outright
    std::os::unix::fs::symlink(&victim, store_root.join("sha256")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("f"), "x").unwrap();
    let tree = snapshot::scan_tree(&source, GitFilter::ExcludeGit).unwrap();
    let error = store.commit_tree(&tree).unwrap_err();
    assert!(format!("{error:#}").contains("refusing to traverse"));
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_store_root_is_refused() {
    let temp = tempfile::tempdir().unwrap();

    // a real store with content, reachable through a symlinked root
    let real_store = temp.path().join("real-store");
    let store_at_real = Store::new(&real_store);
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("f"), "x").unwrap();
    let tree = snapshot::scan_tree(&source, GitFilter::ExcludeGit).unwrap();
    let hash = store_at_real.commit_tree(&tree).unwrap();

    let linked_root = temp.path().join("store");
    std::os::unix::fs::symlink(&real_store, &linked_root).unwrap();
    let store = Store::new(&linked_root);

    // prune warns once and touches nothing behind the link
    let warnings = store.prune(&BTreeSet::new());
    assert_eq!(warnings.len(), 1, "expected one warning: {warnings:?}");
    assert!(warnings[0].contains("refusing to traverse"));
    assert_eq!(
      store_at_real.verify_snapshot(&hash).unwrap(),
      SnapshotStatus::Valid,
      "the unreferenced snapshot must survive"
    );

    // verification and removal refuse the redirected root
    assert!(store.verify_snapshot(&hash).is_err());
    assert!(store.remove_snapshot(&hash).is_err());
  }

  #[test]
  #[cfg(unix)]
  fn verify_repairs_writable_mode_drift() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let hash = commit(&fixture);
    let path = fixture.store.snapshot_path(&hash).unwrap();

    // drift the modes without touching content
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(path.join("docs"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(
      path.join("docs/guide.md"),
      fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    assert_eq!(
      fixture.store.verify_snapshot(&hash).unwrap(),
      SnapshotStatus::Valid
    );

    // Valid means the drift was repaired, not ignored
    assert_eq!(mode_of(&path), 0o555);
    assert_eq!(mode_of(&path.join("docs")), 0o555);
    assert_eq!(mode_of(&path.join("docs/guide.md")), 0o444);
  }

  #[test]
  fn prune_on_an_empty_store_is_a_no_op() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::new(&temp.path().join("never-created"));
    assert!(store.prune(&BTreeSet::new()).is_empty());
  }
}
