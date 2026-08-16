#![allow(dead_code)] // consumed by the command tickets

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// In-process staging, commit, and rollback. There is deliberately no
/// persistent journal or background recovery: a crash mid-commit may require
/// rerunning the command, but every individual write is atomic, so metadata
/// is never partially visible. Preconditions are re-checked immediately
/// before each step, which catches ordinary external edits; per README §10,
/// a same-user process deliberately racing these checks is out of scope.
#[derive(Debug, Default)]
pub struct Transaction {
  operations: Vec<Operation>,
}

/// What the transaction expects to find at a file path at commit time.
/// Anything else means the file changed since planning, and the commit aborts.
#[derive(Debug, Clone, PartialEq)]
pub enum ExpectedFile {
  Absent,
  Bytes(Vec<u8>),
}

/// What the transaction expects to find at a symlink path at commit time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpectedLink {
  Absent,
  /// Any symlink, correct or dangling; its destination is never touched.
  AnySymlink,
}

#[derive(Debug)]
enum Operation {
  CreateDirs {
    path: PathBuf,
  },
  WriteFile {
    path: PathBuf,
    contents: Vec<u8>,
    expected: ExpectedFile,
  },
  SetSymlink {
    path: PathBuf,
    destination: PathBuf,
    expected: ExpectedLink,
  },
  RemoveSymlink {
    path: PathBuf,
    expected: ExpectedLink,
  },
}

/// Reverse actions, applied last-staged-first on failure.
#[derive(Debug)]
enum Undo {
  RemoveFile { path: PathBuf },
  RestoreFile { path: PathBuf, bytes: Vec<u8> },
  RemoveLink { path: PathBuf },
  RestoreLink { path: PathBuf, destination: PathBuf },
  RemoveCreatedDirs { dirs: Vec<PathBuf> },
}

impl Transaction {
  pub fn new() -> Self {
    Self::default()
  }

  /// Creates `path` and any missing ancestors; rollback removes only the
  /// directories this transaction created that are still empty.
  pub fn create_dirs(&mut self, path: &Path) {
    self.operations.push(Operation::CreateDirs {
      path: path.to_path_buf(),
    });
  }

  pub fn write_file(&mut self, path: &Path, contents: Vec<u8>, expected: ExpectedFile) {
    self.operations.push(Operation::WriteFile {
      path: path.to_path_buf(),
      contents,
      expected,
    });
  }

  pub fn set_symlink(&mut self, path: &Path, destination: &Path, expected: ExpectedLink) {
    self.operations.push(Operation::SetSymlink {
      path: path.to_path_buf(),
      destination: destination.to_path_buf(),
      expected,
    });
  }

  /// Removes a symlink, or (with ExpectedLink::Absent) merely re-verifies at
  /// commit time that the planned-missing target is still missing. Anything
  /// that is not a symlink aborts the commit.
  pub fn remove_symlink(&mut self, path: &Path, expected: ExpectedLink) {
    self.operations.push(Operation::RemoveSymlink {
      path: path.to_path_buf(),
      expected,
    });
  }

  pub fn commit(self) -> Result<()> {
    self.commit_with_hook(&mut |_| Ok(()))
  }

  /// The hook runs before each step; tests use it to inject a failure at an
  /// exact commit position.
  fn commit_with_hook(self, hook: &mut dyn FnMut(usize) -> Result<()>) -> Result<()> {
    let mut undos: Vec<Undo> = Vec::new();

    for (index, operation) in self.operations.iter().enumerate() {
      let result = hook(index).and_then(|()| apply(operation, &mut undos));
      if let Err(error) = result {
        let warnings = rollback(&mut undos);
        if warnings.is_empty() {
          return Err(error);
        }
        return Err(error.context(format!("rollback warnings: {}", warnings.join("; "))));
      }
    }

    Ok(())
  }
}

fn apply(operation: &Operation, undos: &mut Vec<Undo>) -> Result<()> {
  match operation {
    Operation::CreateDirs { path } => {
      // registered before creating, so directories made before a mid-chain
      // failure still roll back
      undos.push(Undo::RemoveCreatedDirs { dirs: Vec::new() });
      let Some(Undo::RemoveCreatedDirs { dirs }) = undos.last_mut() else {
        unreachable!("just pushed");
      };
      create_tracked_dirs(path, dirs)
    }

    Operation::WriteFile {
      path,
      contents,
      expected,
    } => {
      // write through the resolved target so a symlinked config stays a
      // symlink; renaming over the logical path would replace the link
      let path = &crate::paths::resolve_real_path(path)?;
      let current = read_optional(path)?;
      let matches = match (expected, &current) {
        (ExpectedFile::Absent, None) => true,
        (ExpectedFile::Bytes(bytes), Some(current)) => bytes == current,
        _ => false,
      };
      if !matches {
        bail!(
          "{} was modified by another process; aborting without changes",
          path.display()
        );
      }

      // undo first, so a half-performed step still rolls back cleanly
      undos.push(match current {
        None => Undo::RemoveFile { path: path.clone() },
        Some(bytes) => Undo::RestoreFile {
          path: path.clone(),
          bytes,
        },
      });
      atomic_write(path, contents)
    }

    Operation::SetSymlink {
      path,
      destination,
      expected,
    } => {
      let state = link_state(path)?;
      match (expected, state) {
        (ExpectedLink::Absent, LinkState::Absent) => {
          undos.push(Undo::RemoveLink { path: path.clone() });
        }
        (ExpectedLink::AnySymlink, LinkState::Symlink(prior)) => {
          undos.push(Undo::RestoreLink {
            path: path.clone(),
            destination: prior,
          });
        }
        _ => bail!(
          "{} changed since it was planned; aborting without changes",
          path.display()
        ),
      }
      set_symlink_atomic(path, destination)
    }

    Operation::RemoveSymlink { path, expected } => {
      match (expected, link_state(path)?) {
        // planned unlink; a link that vanished meanwhile is already removed
        (ExpectedLink::AnySymlink, LinkState::Absent) => Ok(()),
        (ExpectedLink::AnySymlink, LinkState::Symlink(prior)) => {
          undos.push(Undo::RestoreLink {
            path: path.clone(),
            destination: prior,
          });
          fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        // planned-missing target: re-verified at commit, never touched
        (ExpectedLink::Absent, LinkState::Absent) => Ok(()),
        (ExpectedLink::Absent, LinkState::Symlink(_)) => bail!(
          "{} changed since it was planned; aborting without changes",
          path.display()
        ),
        (_, LinkState::Other) => {
          bail!("{} is not a symlink; refusing to remove it", path.display())
        }
      }
    }
  }
}

/// Best-effort, last-in-first-out. Problems come back as warnings attached
/// to the original error; rollback itself never panics or aborts early.
fn rollback(undos: &mut Vec<Undo>) -> Vec<String> {
  let mut warnings = Vec::new();

  while let Some(undo) = undos.pop() {
    let result = match &undo {
      Undo::RemoveFile { path } => {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
      }
      Undo::RestoreFile { path, bytes } => atomic_write(path, bytes),
      Undo::RemoveLink { path } => {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
      }
      Undo::RestoreLink { path, destination } => set_symlink_atomic(path, destination),
      Undo::RemoveCreatedDirs { dirs } => {
        remove_still_empty_dirs(dirs, &mut warnings);
        Ok(())
      }
    };

    if let Err(error) = result {
      warnings.push(format!("{error:#}"));
    }
  }

  warnings
}

/// Deepest-first; a directory that gained content since creation is left
/// alone by design, as is anything the transaction did not create.
fn remove_still_empty_dirs(dirs: &[PathBuf], warnings: &mut Vec<String>) {
  for dir in dirs.iter().rev() {
    match fs::remove_dir(dir) {
      Ok(()) => {}
      Err(error)
        if matches!(
          error.kind(),
          io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
        ) => {}
      Err(error) => warnings.push(format!("failed to remove {}: {error}", dir.display())),
    }
  }
}

enum LinkState {
  Absent,
  Symlink(PathBuf),
  Other,
}

fn link_state(path: &Path) -> Result<LinkState> {
  match fs::symlink_metadata(path) {
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LinkState::Absent),
    Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    Ok(metadata) if metadata.file_type().is_symlink() => {
      let destination = fs::read_link(path)
        .with_context(|| format!("failed to read symlink {}", path.display()))?;
      Ok(LinkState::Symlink(destination))
    }
    Ok(_) => Ok(LinkState::Other),
  }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
  match fs::read(path) {
    Ok(bytes) => Ok(Some(bytes)),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
  }
}

/// Temporary sibling plus rename; existing permissions survive an overwrite.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
  let parent = path
    .parent()
    .with_context(|| format!("{} has no parent directory", path.display()))?;

  let mut temp = tempfile::NamedTempFile::new_in(parent)
    .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
  temp.write_all(contents)?;

  if let Ok(metadata) = fs::metadata(path) {
    temp.as_file().set_permissions(metadata.permissions())?;
  }

  temp.as_file().sync_all()?;
  temp
    .persist(path)
    .with_context(|| format!("failed to write {}", path.display()))?;
  Ok(())
}

/// Atomically creates or replaces a symlink without touching whatever the
/// old link pointed at.
fn set_symlink_atomic(path: &Path, destination: &Path) -> Result<()> {
  #[cfg(unix)]
  {
    let parent = path
      .parent()
      .with_context(|| format!("{} has no parent directory", path.display()))?;

    for attempt in 0..100u32 {
      let temp = parent.join(format!(".spm-link-{}-{attempt}", std::process::id()));
      match std::os::unix::fs::symlink(destination, &temp) {
        Ok(()) => {
          return fs::rename(&temp, path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            anyhow::Error::from(error)
              .context(format!("failed to install symlink {}", path.display()))
          });
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
        Err(error) => {
          return Err(error).with_context(|| {
            format!("failed to create temporary symlink in {}", parent.display())
          });
        }
      }
    }
    bail!(
      "could not find a free temporary symlink name in {}",
      parent.display()
    );
  }

  #[cfg(not(unix))]
  bail!("cannot create symlink {} on this platform", path.display())
}

/// Creates every missing ancestor one by one, appending each success to
/// `created` immediately so even a mid-chain failure leaves a full record
/// for rollback.
fn create_tracked_dirs(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
  let mut missing = Vec::new();
  let mut current = path;

  loop {
    // fs::metadata follows symlinks: an existing symlinked parent is allowed
    match fs::metadata(current) {
      Ok(metadata) if metadata.is_dir() => break,
      Ok(_) => bail!("{} exists but is not a directory", current.display()),
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        missing.push(current.to_path_buf());
        let Some(parent) = current.parent() else {
          bail!("cannot create directory {}", path.display());
        };
        current = parent;
      }
      Err(error) => {
        return Err(error).with_context(|| format!("failed to stat {}", current.display()));
      }
    }
  }

  missing.reverse(); // shallowest first
  create_recording(&missing, created)
}

fn create_recording(missing: &[PathBuf], created: &mut Vec<PathBuf>) -> Result<()> {
  for dir in missing {
    fs::create_dir(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    created.push(dir.clone());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn read_link(path: &Path) -> PathBuf {
    fs::read_link(path).unwrap()
  }

  /// A three-op environment: overwrite an existing file, replace an existing
  /// link, create a fresh link under new parents.
  struct World {
    temp: tempfile::TempDir,
  }

  impl World {
    fn root(&self) -> &Path {
      self.temp.path()
    }

    fn config(&self) -> PathBuf {
      self.root().join("spm.toml")
    }

    fn old_link(&self) -> PathBuf {
      self.root().join("targets/existing")
    }

    fn new_link(&self) -> PathBuf {
      self.root().join("targets/deep/nested/fresh")
    }

    fn transaction(&self) -> Transaction {
      let mut tx = Transaction::new();
      tx.write_file(
        &self.config(),
        b"new config".to_vec(),
        ExpectedFile::Bytes(b"old config".to_vec()),
      );
      tx.set_symlink(
        &self.old_link(),
        Path::new("/store/new-snapshot"),
        ExpectedLink::AnySymlink,
      );
      tx.create_dirs(self.new_link().parent().unwrap());
      tx.set_symlink(
        &self.new_link(),
        Path::new("/store/new-snapshot"),
        ExpectedLink::Absent,
      );
      tx
    }

    fn assert_pristine(&self) {
      assert_eq!(fs::read(self.config()).unwrap(), b"old config");
      assert_eq!(
        read_link(&self.old_link()),
        PathBuf::from("/store/old-snapshot")
      );
      assert!(!self.new_link().exists());
      assert!(
        !self.root().join("targets/deep").exists(),
        "created parents must be rolled back"
      );
      self.assert_no_temp_litter();
    }

    fn assert_committed(&self) {
      assert_eq!(fs::read(self.config()).unwrap(), b"new config");
      assert_eq!(
        read_link(&self.old_link()),
        PathBuf::from("/store/new-snapshot")
      );
      assert_eq!(
        read_link(&self.new_link()),
        PathBuf::from("/store/new-snapshot")
      );
      self.assert_no_temp_litter();
    }

    fn assert_no_temp_litter(&self) {
      let mut stack = vec![self.root().to_path_buf()];
      while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
          let entry = entry.unwrap();
          let name = entry.file_name().to_string_lossy().into_owned();
          assert!(
            !name.starts_with(".spm-link-") && !name.starts_with(".tmp"),
            "temporary artifact left behind: {}",
            entry.path().display()
          );
          if entry.file_type().unwrap().is_dir() {
            stack.push(entry.path());
          }
        }
      }
    }
  }

  fn world() -> World {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("spm.toml"), b"old config").unwrap();
    fs::create_dir(temp.path().join("targets")).unwrap();
    std::os::unix::fs::symlink("/store/old-snapshot", temp.path().join("targets/existing"))
      .unwrap();
    World { temp }
  }

  #[test]
  fn a_full_commit_applies_every_operation() {
    let world = world();
    world.transaction().commit().unwrap();
    world.assert_committed();
  }

  #[test]
  fn every_step_failure_restores_the_prior_visible_state() {
    // step count = 4 ops; failing each position must leave the world pristine
    for failing_step in 0..4 {
      let world = world();
      let error = world
        .transaction()
        .commit_with_hook(&mut |step| {
          if step == failing_step {
            bail!("injected failure at step {step}");
          }
          Ok(())
        })
        .unwrap_err();

      assert!(error.to_string().contains("injected failure"));
      world.assert_pristine();
    }
  }

  #[test]
  fn externally_edited_files_abort_the_commit() {
    let world = world();
    fs::write(world.config(), b"edited behind spm's back").unwrap();

    let error = world.transaction().commit().unwrap_err();
    assert!(error.to_string().contains("modified by another process"));
    assert_eq!(
      fs::read(world.config()).unwrap(),
      b"edited behind spm's back"
    );
    assert_eq!(
      read_link(&world.old_link()),
      PathBuf::from("/store/old-snapshot")
    );
  }

  #[test]
  fn unexpected_entry_types_abort_the_commit() {
    let world = world();
    // planning expected a symlink; a regular file appeared instead
    fs::remove_file(world.old_link()).unwrap();
    fs::write(world.old_link(), "a real file").unwrap();

    let error = world.transaction().commit().unwrap_err();
    assert!(error.to_string().contains("changed since it was planned"));

    // the config write (step 0) was rolled back; the imposter is untouched
    assert_eq!(fs::read(world.config()).unwrap(), b"old config");
    assert_eq!(fs::read_to_string(world.old_link()).unwrap(), "a real file");
  }

  #[test]
  fn removing_symlinks_restores_them_on_failure() {
    let world = world();

    let mut tx = Transaction::new();
    tx.remove_symlink(&world.old_link(), ExpectedLink::AnySymlink);
    tx.write_file(&world.config(), b"x".to_vec(), ExpectedFile::Absent); // fails: file exists

    tx.commit().unwrap_err();
    assert_eq!(
      read_link(&world.old_link()),
      PathBuf::from("/store/old-snapshot")
    );
  }

  #[test]
  fn removing_a_missing_symlink_is_accepted() {
    let world = world();
    let mut tx = Transaction::new();
    tx.remove_symlink(
      &world.root().join("targets/never-existed"),
      ExpectedLink::Absent,
    );
    tx.commit().unwrap();
  }

  #[test]
  fn planned_missing_removals_recheck_at_commit() {
    let world = world();
    let planned_missing = world.root().join("targets/was-missing");

    let mut tx = Transaction::new();
    tx.remove_symlink(&world.old_link(), ExpectedLink::AnySymlink);
    tx.remove_symlink(&planned_missing, ExpectedLink::Absent);

    // a regular file appears at the planned-missing target before commit
    fs::write(&planned_missing, "user data").unwrap();

    let error = tx.commit().unwrap_err();
    assert!(error.to_string().contains("refusing to remove"));

    // the earlier unlink was rolled back and the new file is untouched
    assert_eq!(
      read_link(&world.old_link()),
      PathBuf::from("/store/old-snapshot")
    );
    assert_eq!(fs::read_to_string(&planned_missing).unwrap(), "user data");

    // a link appearing where none was planned aborts too
    let mut tx = Transaction::new();
    tx.remove_symlink(&world.root().join("targets/surprise"), ExpectedLink::Absent);
    std::os::unix::fs::symlink("/somewhere", world.root().join("targets/surprise")).unwrap();
    let error = tx.commit().unwrap_err();
    assert!(error.to_string().contains("changed since it was planned"));
    assert!(
      fs::symlink_metadata(world.root().join("targets/surprise")).is_ok(),
      "the surprise link must not be unlinked"
    );
  }

  #[test]
  fn removing_a_non_symlink_is_refused() {
    let world = world();
    let mut tx = Transaction::new();
    tx.remove_symlink(&world.config(), ExpectedLink::AnySymlink);

    let error = tx.commit().unwrap_err();
    assert!(error.to_string().contains("refusing to remove"));
    assert_eq!(fs::read(world.config()).unwrap(), b"old config");
  }

  #[test]
  fn replacing_a_symlink_never_touches_its_destination() {
    let world = world();
    let destination = world.root().join("real-dest");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("precious"), "data").unwrap();

    fs::remove_file(world.old_link()).unwrap();
    std::os::unix::fs::symlink(&destination, world.old_link()).unwrap();

    let mut tx = Transaction::new();
    tx.set_symlink(
      &world.old_link(),
      Path::new("/store/new-snapshot"),
      ExpectedLink::AnySymlink,
    );
    tx.commit().unwrap();

    assert_eq!(
      read_link(&world.old_link()),
      PathBuf::from("/store/new-snapshot")
    );
    assert_eq!(
      fs::read_to_string(destination.join("precious")).unwrap(),
      "data"
    );
  }

  #[test]
  fn rollback_spares_preexisting_and_nonempty_directories() {
    let world = world();
    let preexisting = world.root().join("targets/keep");
    fs::create_dir(&preexisting).unwrap();
    fs::write(preexisting.join("occupant"), "here first").unwrap();

    let mut tx = Transaction::new();
    // creates keep/created-a/created-b; "keep" itself already exists
    tx.create_dirs(&preexisting.join("created-a/created-b"));
    tx.write_file(&world.config(), b"x".to_vec(), ExpectedFile::Absent); // fails

    tx.commit_with_hook(&mut |_| Ok(())).unwrap_err();

    assert!(preexisting.exists(), "preexisting parent must survive");
    assert_eq!(
      fs::read_to_string(preexisting.join("occupant")).unwrap(),
      "here first"
    );
    assert!(!preexisting.join("created-a").exists());
  }

  #[test]
  fn rollback_leaves_created_directories_that_gained_content() {
    let world = world();
    let target_dir = world.root().join("targets/created");

    let mut tx = Transaction::new();
    tx.create_dirs(&target_dir);
    tx.write_file(&world.config(), b"x".to_vec(), ExpectedFile::Absent); // fails

    // someone drops a file into the created directory mid-commit
    let dir = target_dir.clone();
    let mut step = 0;
    tx.commit_with_hook(&mut |index| {
      step = index;
      if index == 1 {
        fs::write(dir.join("squatter"), "moved in").unwrap();
      }
      Ok(())
    })
    .unwrap_err();

    assert!(
      target_dir.join("squatter").exists(),
      "a created directory that gained content must not be deleted"
    );
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_files_are_written_through_and_survive_rollback() {
    let world = world();
    let real = world.root().join("real-config.toml");
    fs::write(&real, b"old config").unwrap();
    let link = world.root().join("linked-config.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let assert_still_linked = |world: &World| {
      let meta = fs::symlink_metadata(&link).unwrap();
      assert!(meta.file_type().is_symlink(), "the symlink must survive");
      assert_eq!(fs::read_link(&link).unwrap(), real);
      let _ = world;
    };

    // failure: rollback restores the real file's bytes, not a new file
    let mut tx = Transaction::new();
    tx.write_file(
      &link,
      b"new config".to_vec(),
      ExpectedFile::Bytes(b"old config".to_vec()),
    );
    tx.remove_symlink(&world.config(), ExpectedLink::AnySymlink); // fails: spm.toml is a regular file
    tx.commit().unwrap_err();
    assert_still_linked(&world);
    assert_eq!(fs::read(&real).unwrap(), b"old config");

    // success: the write lands in the real file behind the link
    let mut tx = Transaction::new();
    tx.write_file(
      &link,
      b"new config".to_vec(),
      ExpectedFile::Bytes(b"old config".to_vec()),
    );
    tx.commit().unwrap();
    assert_still_linked(&world);
    assert_eq!(fs::read(&real).unwrap(), b"new config");
  }

  #[test]
  fn partially_created_directories_are_recorded_for_rollback() {
    // a mid-chain create_dir failure (here: the second entry lands under a
    // regular file) must still leave the earlier success in the undo record
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("created-ok");
    fs::write(temp.path().join("wall"), "a file").unwrap();

    let mut created = Vec::new();
    let result = create_recording(
      &[first.clone(), temp.path().join("wall/impossible")],
      &mut created,
    );

    assert!(result.is_err(), "creating under a file must fail");
    assert_eq!(
      created,
      vec![first.clone()],
      "the successful step is recorded"
    );

    // and rollback removes exactly the recorded directory
    let mut warnings = Vec::new();
    remove_still_empty_dirs(&created, &mut warnings);
    assert!(warnings.is_empty());
    assert!(!first.exists());
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_parents_are_allowed_for_created_dirs() {
    let world = world();
    let real = world.root().join("real-parent");
    fs::create_dir(&real).unwrap();
    let linked = world.root().join("linked-parent");
    std::os::unix::fs::symlink(&real, &linked).unwrap();

    let mut tx = Transaction::new();
    tx.create_dirs(&linked.join("child"));
    tx.commit().unwrap();

    assert!(real.join("child").is_dir());
  }
}
