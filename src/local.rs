use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths;
use crate::skill;
use crate::snapshot::{self, GitFilter};
use crate::source::LocalSource;
use crate::store::{SnapshotStatus, Store};

/// Everything a command needs to lock and install a local skill. Preparation
/// writes only to the store — never config, lock, or targets.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLocalSkill {
  pub name: String,
  pub description: String,
  pub content_hash: String,
  pub resolved_path: PathBuf,
  /// Whether this preparation created the snapshot (vs reusing the store);
  /// failed commands may clean up only created snapshots.
  pub created: bool,
}

/// Validates, snapshots, and commits a local source. Reuses the store when
/// the current source hash already has a valid snapshot.
pub fn prepare_local_skill(
  store: &Store,
  source: &LocalSource,
  home: &Path,
) -> Result<PreparedLocalSkill> {
  prepare_with_hook(store, source, home, || ())
}

/// `after_commit` runs between the store commit and the post-staging source
/// re-hash; tests use it to simulate a source mutating mid-preparation.
fn prepare_with_hook(
  store: &Store,
  source: &LocalSource,
  home: &Path,
  after_commit: impl FnOnce(),
) -> Result<PreparedLocalSkill> {
  let resolved = resolve_source_dir(&source.path, home)?;

  // fail fast on an obviously invalid skill before paying for staging; the
  // returned metadata is deliberately NOT taken from this read
  skill::load_skill_metadata(&resolved)?;

  let tree = snapshot::scan_tree(&resolved, GitFilter::ExcludeGit)?;
  let source_hash = snapshot::hash_tree(&tree)?;

  // unchanged fast path: the store already holds this exact content
  if store.verify_snapshot(&source_hash)? == SnapshotStatus::Valid {
    return prepared_from_snapshot(store, source_hash, resolved, false);
  }

  let committed = store.commit_tree(&tree)?;
  after_commit();

  // hash the source again after staging; a mid-preparation edit means the
  // snapshot may be a torn mix of old and new content
  let post_tree = snapshot::scan_tree(&resolved, GitFilter::ExcludeGit)?;
  if snapshot::hash_tree(&post_tree)? != committed.content_hash {
    // only a snapshot this commit created; a deduplicated one may be
    // referenced by other installed skills
    if committed.created {
      let _ = store.remove_snapshot(&committed.content_hash);
    }
    bail!(
      "local source {} changed while skillpm was preparing it; rerun the command",
      resolved.display()
    );
  }

  prepared_from_snapshot(store, committed.content_hash, resolved, committed.created)
}

/// Metadata is read from the immutable, hash-verified snapshot — never the
/// live source — so what a command locks can never disagree with what was
/// snapshotted. An invalid snapshot is removed rather than left committed.
fn prepared_from_snapshot(
  store: &Store,
  content_hash: String,
  resolved_path: PathBuf,
  created: bool,
) -> Result<PreparedLocalSkill> {
  let snapshot_dir = store.snapshot_path(&content_hash)?;
  let metadata = match skill::load_skill_metadata(&snapshot_dir) {
    Ok(metadata) => metadata,
    Err(error) => {
      let _ = store.remove_snapshot(&content_hash);
      return Err(error);
    }
  };

  Ok(PreparedLocalSkill {
    name: metadata.name,
    description: metadata.description,
    content_hash,
    resolved_path,
    created,
  })
}

/// For `install`: a missing or corrupt locked snapshot may be rebuilt only
/// while the current source still hashes to the locked value.
pub fn reconstruct_local_snapshot(
  store: &Store,
  source: &LocalSource,
  home: &Path,
  locked_hash: &str,
) -> Result<()> {
  let resolved = resolve_source_dir(&source.path, home)?;

  let tree = snapshot::scan_tree(&resolved, GitFilter::ExcludeGit)?;
  let current_hash = snapshot::hash_tree(&tree)?;
  if current_hash != locked_hash {
    bail!(
      "local source {} no longer matches its locked snapshot; run `skillpm update`",
      resolved.display()
    );
  }

  let committed = store.commit_tree(&tree)?;
  if committed.content_hash != locked_hash {
    if committed.created {
      let _ = store.remove_snapshot(&committed.content_hash);
    }
    bail!(
      "local source {} changed while skillpm was reconstructing it; rerun the command",
      resolved.display()
    );
  }

  Ok(())
}

/// Resolution allows symlinked parents (canonicalize follows them); the
/// resolved source must exist and be a directory.
fn resolve_source_dir(path: &Path, home: &Path) -> Result<PathBuf> {
  let resolved = paths::resolve_user_path(path, home)?;

  let canonical = match fs::canonicalize(&resolved) {
    Ok(canonical) => canonical,
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      bail!("local source {} does not exist", resolved.display());
    }
    Err(error) => {
      return Err(error)
        .with_context(|| format!("failed to resolve local source {}", resolved.display()));
    }
  };

  if !canonical.is_dir() {
    bail!("local source {} must be a directory", canonical.display());
  }

  Ok(canonical)
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Fixture {
    temp: tempfile::TempDir,
    store: Store,
    home: PathBuf,
  }

  impl Fixture {
    fn source(&self) -> LocalSource {
      LocalSource {
        path: PathBuf::from("skills/my-skill"),
      }
    }

    fn source_dir(&self) -> PathBuf {
      self.home.join("skills/my-skill")
    }

    fn prepare(&self) -> Result<PreparedLocalSkill> {
      prepare_local_skill(&self.store, &self.source(), &self.home)
    }
  }

  fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let source_dir = home.join("skills/my-skill");

    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
      source_dir.join("SKILL.md"),
      "---\nname: my-skill\ndescription: Does things.\n---\n",
    )
    .unwrap();

    Fixture {
      store: Store::new(&temp.path().join("store")),
      temp,
      home,
    }
  }

  #[test]
  fn prepare_validates_snapshots_and_commits() {
    let fixture = fixture();
    let prepared = fixture.prepare().unwrap();

    assert_eq!(prepared.name, "my-skill");
    assert_eq!(prepared.description, "Does things.");
    assert_eq!(
      prepared.resolved_path,
      fixture.source_dir().canonicalize().unwrap()
    );
    assert_eq!(
      fixture
        .store
        .verify_snapshot(&prepared.content_hash)
        .unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  fn unchanged_sources_reuse_the_store_without_restaging() {
    let fixture = fixture();
    let first = fixture.prepare().unwrap();

    let mut committed_again = false;
    let second = prepare_with_hook(&fixture.store, &fixture.source(), &fixture.home, || {
      committed_again = true;
    })
    .unwrap();

    assert_eq!(first.content_hash, second.content_hash);
    assert!(
      !committed_again,
      "unchanged content must take the reuse path"
    );
  }

  #[test]
  fn changed_content_produces_a_new_hash() {
    let fixture = fixture();
    let first = fixture.prepare().unwrap();

    fs::write(fixture.source_dir().join("extra.md"), "more\n").unwrap();
    let second = fixture.prepare().unwrap();

    assert_ne!(first.content_hash, second.content_hash);
    assert_eq!(
      fixture.store.verify_snapshot(&second.content_hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  fn mutation_during_preparation_aborts_and_leaves_no_orphan() {
    let fixture = fixture();

    // the hook fires after commit; the abort condition (post-hash differs
    // from the committed hash) is the same one a mid-staging edit trips
    let error = prepare_with_hook(&fixture.store, &fixture.source(), &fixture.home, || {
      fs::write(fixture.source_dir().join("raced.md"), "surprise\n").unwrap();
    })
    .unwrap_err();

    assert!(
      error
        .to_string()
        .contains("changed while skillpm was preparing")
    );

    // the possibly-torn snapshot must not stay committed
    let sha256_dir = fixture.temp.path().join("store/sha256");
    let leftovers = fs::read_dir(&sha256_dir).unwrap().count();
    assert_eq!(leftovers, 0, "aborted preparation left a snapshot behind");
  }

  #[test]
  fn missing_and_non_directory_sources_are_rejected() {
    let fixture = fixture();

    let missing = LocalSource {
      path: PathBuf::from("skills/absent"),
    };
    let error = prepare_local_skill(&fixture.store, &missing, &fixture.home).unwrap_err();
    assert!(error.to_string().contains("does not exist"));

    fs::write(fixture.home.join("skills/file"), "not a dir").unwrap();
    let file = LocalSource {
      path: PathBuf::from("skills/file"),
    };
    let error = prepare_local_skill(&fixture.store, &file, &fixture.home).unwrap_err();
    assert!(error.to_string().contains("must be a directory"));
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_source_parents_are_followed() {
    let fixture = fixture();
    std::os::unix::fs::symlink(fixture.home.join("skills"), fixture.home.join("linked")).unwrap();

    let through_link = LocalSource {
      path: PathBuf::from("linked/my-skill"),
    };
    let prepared = prepare_local_skill(&fixture.store, &through_link, &fixture.home).unwrap();

    assert_eq!(
      prepared.resolved_path,
      fixture.source_dir().canonicalize().unwrap()
    );
  }

  #[test]
  fn git_is_omitted_while_hidden_files_and_node_modules_remain() {
    let fixture = fixture();
    let dir = fixture.source_dir();
    fs::create_dir(dir.join(".git")).unwrap();
    fs::write(dir.join(".git/config"), "gitdata").unwrap();
    fs::write(dir.join(".hidden"), "kept").unwrap();
    fs::create_dir(dir.join("node_modules")).unwrap();
    fs::write(dir.join("node_modules/dep.js"), "kept").unwrap();

    let prepared = fixture.prepare().unwrap();
    let snapshot_path = fixture.store.snapshot_path(&prepared.content_hash).unwrap();

    assert!(!snapshot_path.join(".git").exists());
    assert!(snapshot_path.join(".hidden").exists());
    assert!(snapshot_path.join("node_modules/dep.js").exists());
  }

  #[test]
  fn reconstruction_is_exact_or_refused() {
    let fixture = fixture();
    let prepared = fixture.prepare().unwrap();

    // simulate a deleted store cache
    fixture
      .store
      .remove_snapshot(&prepared.content_hash)
      .unwrap();
    assert_eq!(
      fixture
        .store
        .verify_snapshot(&prepared.content_hash)
        .unwrap(),
      SnapshotStatus::Missing
    );

    reconstruct_local_snapshot(
      &fixture.store,
      &fixture.source(),
      &fixture.home,
      &prepared.content_hash,
    )
    .unwrap();
    assert_eq!(
      fixture
        .store
        .verify_snapshot(&prepared.content_hash)
        .unwrap(),
      SnapshotStatus::Valid
    );

    // a drifted source can no longer reconstruct the locked snapshot
    fixture
      .store
      .remove_snapshot(&prepared.content_hash)
      .unwrap();
    fs::write(fixture.source_dir().join("drift.md"), "changed\n").unwrap();
    let error = reconstruct_local_snapshot(
      &fixture.store,
      &fixture.source(),
      &fixture.home,
      &prepared.content_hash,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no longer matches"));
  }

  #[test]
  fn preparation_touches_nothing_outside_the_store() {
    let fixture = fixture();

    // stand-ins for config, lock, and an installed target
    let config = fixture.temp.path().join("skillpm.toml");
    let lock = fixture.temp.path().join("skillpm.lock");
    fs::write(&config, "version = 1\n").unwrap();
    fs::write(&lock, "version = 1\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("somewhere", fixture.temp.path().join("target-link")).unwrap();

    fixture.prepare().unwrap();

    assert_eq!(fs::read_to_string(&config).unwrap(), "version = 1\n");
    assert_eq!(fs::read_to_string(&lock).unwrap(), "version = 1\n");
    #[cfg(unix)]
    assert_eq!(
      fs::read_link(fixture.temp.path().join("target-link")).unwrap(),
      PathBuf::from("somewhere")
    );

    // and the source itself only ever gets read
    let names: Vec<_> = fs::read_dir(fixture.source_dir())
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("SKILL.md")]);
  }
}
