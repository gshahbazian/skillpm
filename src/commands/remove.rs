use std::collections::BTreeSet;
use std::fmt;

use anyhow::{Result, bail};

use super::CommandEnv;
use crate::config::ConfigDocument;
use crate::lockfile;
use crate::output;
use crate::store::Store;
use crate::targets::{self, RemovalAction};
use crate::transaction::{ExpectedFile, Transaction};

pub fn run(name: String) -> Result<()> {
  let env = CommandEnv::from_process()?;
  let summary = execute(&env, &name)?;
  output::success(&summary.to_string());
  Ok(())
}

#[derive(Debug, PartialEq)]
pub(crate) struct RemoveSummary {
  pub name: String,
  pub unlinked: usize,
  pub already_missing: usize,
  pub prune_warnings: usize,
}

impl fmt::Display for RemoveSummary {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "removed skill '{}': {} link(s) unlinked, {} already missing",
      self.name, self.unlinked, self.already_missing
    )
  }
}

pub(crate) fn execute(env: &CommandEnv, name: &str) -> Result<RemoveSummary> {
  execute_with_hook(env, name, &mut || Ok(()))
}

/// The hook runs right before the transaction commits; tests use it to
/// simulate concurrent external edits.
fn execute_with_hook(
  env: &CommandEnv,
  name: &str,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<RemoveSummary> {
  let paths = &env.paths;
  let _lock = super::acquire_lock(paths)?;

  let mut config_doc = ConfigDocument::load(&paths.config_file)?;
  let config = config_doc.config()?;
  let lock_doc = lockfile::require_fresh(&paths.lockfile, &config)?;

  if !config.skills.contains_key(name) {
    bail!("skill '{name}' is not configured");
  }

  let protected = vec![paths.config_dir().to_path_buf(), paths.data_root.clone()];
  let plan = targets::plan_targets(&config, &paths.home, &protected)?;

  // preflight and stage every target before anything is unlinked; a regular
  // file or directory at any target aborts the whole removal here
  let mut transaction = Transaction::new();
  let mut summary = RemoveSummary {
    name: name.to_string(),
    unlinked: 0,
    already_missing: 0,
    prune_warnings: 0,
  };
  for target in plan.iter().filter(|target| target.skill == name) {
    match targets::stage_removal(&mut transaction, &target.resolved)? {
      RemovalAction::Unlink => summary.unlinked += 1,
      RemovalAction::AlreadyMissing => summary.already_missing += 1,
    }
  }

  // metadata: drop only this skill's entries; removing the final skill
  // leaves valid empty files
  config_doc.remove_skill(name)?;
  let config_expected = ExpectedFile::Bytes(
    config_doc
      .original_bytes()
      .expect("the config was read from disk")
      .to_vec(),
  );
  transaction.write_file(
    &paths.config_file,
    config_doc.rendered_bytes()?,
    config_expected,
  );

  let mut new_lock = lock_doc.lockfile.clone();
  new_lock.skills.remove(name);
  let lock_expected = ExpectedFile::Bytes(
    lock_doc
      .original_bytes()
      .expect("fresh lock state was read from disk")
      .to_vec(),
  );
  transaction.write_file(
    &paths.lockfile,
    lockfile::render_validated(&new_lock)?,
    lock_expected,
  );

  before_commit()?;

  // re-read metadata bytes immediately before committing (README section 10)
  if config_doc.externally_modified()? {
    bail!("skillpm.toml changed while skillpm was running; aborting without changes");
  }
  if lock_doc.externally_modified()? {
    bail!("skillpm.lock changed while skillpm was running; aborting without changes");
  }

  transaction.commit()?;

  // the removed skill's snapshot is pruned unless still referenced
  let store = Store::new(&paths.store);
  let referenced: BTreeSet<String> = new_lock
    .skills
    .values()
    .map(|entry| entry.content_hash.clone())
    .collect();
  let warnings = store.prune(&referenced);
  summary.prune_warnings = warnings.len();
  for warning in &warnings {
    output::warning(&warning.clone());
  }

  Ok(summary)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::commands::add;
  use crate::testutil::{self, World, write_skill_md};
  use std::fs;
  use std::path::PathBuf;

  // shadows super::execute: absorbs transient test-only lock contention
  fn execute(env: &CommandEnv, name: &str) -> Result<RemoveSummary> {
    testutil::retry_lock(|| super::execute(env, name))
  }

  /// Two installed local skills, each with one target link.
  fn fixture() -> World {
    let world = testutil::world();
    for name in ["skill-one", "skill-two"] {
      write_skill_md(&world.home.join("skills").join(name), name);
      testutil::retry_lock(|| {
        add::execute(
          &world.offline_env(),
          &format!("skills/{name}"),
          &[PathBuf::from(format!("links/{name}"))],
          None,
        )
      })
      .unwrap();
    }
    world
  }

  #[test]
  fn removal_unlinks_and_drops_only_the_named_skill() {
    let world = fixture();
    let summary = execute(&world.offline_env(), "skill-one").unwrap();
    assert_eq!(summary.unlinked, 1);
    assert_eq!(summary.already_missing, 0);

    // the link is gone, its parent directory and the source remain
    assert!(!world.home.join("links/skill-one").exists());
    assert!(world.home.join("links").is_dir());
    assert!(world.home.join("skills/skill-one/SKILL.md").exists());

    // the other skill is untouched, its snapshot retained
    let other = fs::read_link(world.home.join("links/skill-two")).unwrap();
    assert!(other.join("SKILL.md").exists());

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(!config.contains("skill-one"));
    assert!(config.contains("[skills.skill-two]"));
    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    assert!(!lock.contains("skill-one"));

    // the removed skill's snapshot was pruned; the other survives
    let snapshots = fs::read_dir(world.paths().store.join("sha256"))
      .unwrap()
      .count();
    assert_eq!(snapshots, 1);
  }

  #[test]
  #[cfg(unix)]
  fn missing_and_dangling_targets_are_accepted() {
    let world = fixture();

    // one target already gone, replaced later by a dangling link elsewhere
    fs::remove_file(world.home.join("links/skill-one")).unwrap();
    let summary = execute(&world.offline_env(), "skill-one").unwrap();
    assert_eq!(summary.already_missing, 1);
    assert_eq!(summary.unlinked, 0);

    // a dangling link still unlinks
    fs::remove_file(world.home.join("links/skill-two")).unwrap();
    std::os::unix::fs::symlink("/nowhere/at/all", world.home.join("links/skill-two")).unwrap();
    let summary = execute(&world.offline_env(), "skill-two").unwrap();
    assert_eq!(summary.unlinked, 1);
    assert!(!world.home.join("links/skill-two").exists());
  }

  #[test]
  fn a_regular_file_target_aborts_everything() {
    let world = fixture();
    let config_before = world.config_bytes();
    let lock_before = world.lock_bytes();

    fs::remove_file(world.home.join("links/skill-one")).unwrap();
    fs::write(world.home.join("links/skill-one"), "user data").unwrap();

    let error = execute(&world.offline_env(), "skill-one").unwrap_err();
    assert!(error.to_string().contains("refusing to remove"));

    assert_eq!(world.config_bytes(), config_before);
    assert_eq!(world.lock_bytes(), lock_before);
    assert_eq!(
      fs::read_to_string(world.home.join("links/skill-one")).unwrap(),
      "user data"
    );
  }

  #[test]
  fn unknown_names_are_rejected() {
    let world = fixture();
    let error = execute(&world.offline_env(), "nope").unwrap_err();
    assert!(error.to_string().contains("'nope' is not configured"));
  }

  #[test]
  fn removing_the_final_skill_leaves_valid_empty_state() {
    let world = fixture();
    execute(&world.offline_env(), "skill-one").unwrap();
    execute(&world.offline_env(), "skill-two").unwrap();

    assert_eq!(
      String::from_utf8(world.lock_bytes()).unwrap(),
      "version = 1\n"
    );
    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.contains("version = 1"));
    assert!(!config.contains("[skills."));

    // the empty state is fully usable: adding works again
    write_skill_md(&world.home.join("skills/skill-one"), "skill-one");
    testutil::retry_lock(|| {
      add::execute(
        &world.offline_env(),
        "skills/skill-one",
        &[PathBuf::from("links/skill-one")],
        None,
      )
    })
    .unwrap();
  }

  #[test]
  fn comments_survive_removal_of_one_skill() {
    let world = fixture();
    let decorated = format!(
      "# managed by me\n{}",
      String::from_utf8(world.config_bytes()).unwrap()
    );
    world.write_config(&decorated);

    execute(&world.offline_env(), "skill-one").unwrap();

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.starts_with("# managed by me\n"));
    assert!(config.contains("[skills.skill-two]"));
  }

  #[test]
  fn external_edits_mid_removal_roll_back_the_unlinks() {
    let world = fixture();
    let config_path = world.paths().config_file;
    let env = world.offline_env();

    let error = testutil::retry_lock(|| {
      super::execute_with_hook(&env, "skill-one", &mut || {
        let mut bytes = fs::read(&config_path).unwrap();
        bytes.extend_from_slice(b"\n# edited externally\n");
        fs::write(&config_path, bytes).unwrap();
        Ok(())
      })
    })
    .unwrap_err();
    assert!(error.to_string().contains("skillpm.toml changed"));

    // the staged unlink never happened
    assert!(
      fs::symlink_metadata(world.home.join("links/skill-one"))
        .unwrap()
        .file_type()
        .is_symlink(),
      "the link must survive the aborted removal"
    );
  }

  #[test]
  fn shared_snapshots_are_retained_by_the_remaining_reference() {
    // two skills cannot share a hash through the real flow (the hash covers
    // SKILL.md, which carries the unique name), so hand-craft lock state
    // where both entries reference skill-one's snapshot
    let world = fixture();
    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    let hash_of = |name: &str| {
      let mut in_entry = false;
      for line in lock.lines() {
        if line == format!("[skills.{name}]") {
          in_entry = true;
        } else if line.starts_with("[skills.") {
          in_entry = false;
        } else if in_entry && let Some(hash) = line.strip_prefix("content_hash = \"") {
          return hash.trim_end_matches('"').to_string();
        }
      }
      panic!("no hash for {name}");
    };
    let (one, two) = (hash_of("skill-one"), hash_of("skill-two"));
    fs::write(world.paths().lockfile, lock.replace(&two, &one)).unwrap();

    execute(&world.offline_env(), "skill-two").unwrap();

    // skill-one still references the shared snapshot: prune must keep it
    let store = world.store();
    assert_eq!(
      store.verify_snapshot(&one).unwrap(),
      crate::store::SnapshotStatus::Valid,
      "the shared snapshot must survive"
    );
  }

  #[test]
  #[cfg(unix)]
  fn prune_problems_are_warnings_not_failures() {
    let world = fixture();
    let staging = world.paths().store.join("staging");
    let _ = fs::remove_dir_all(&staging);
    std::os::unix::fs::symlink(world.temp.path(), &staging).unwrap();

    let summary = execute(&world.offline_env(), "skill-one").unwrap();
    assert!(summary.prune_warnings > 0, "expected a prune warning");
  }

  #[test]
  #[cfg(unix)]
  fn commit_failures_after_unlinking_restore_the_links() {
    use std::os::unix::fs::PermissionsExt;

    let world = fixture();
    let config_dir = world.paths().config_file.parent().unwrap().to_path_buf();
    let env = world.offline_env();

    // sabotage the config write (staged AFTER the unlinks): its temp-sibling
    // creation fails once the directory is read-only, so the commit fails
    // with the unlink already applied and must roll it back
    let error = testutil::retry_lock(|| {
      super::execute_with_hook(&env, "skill-one", &mut || {
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o555)).unwrap();
        Ok(())
      })
    })
    .unwrap_err();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
      format!("{error:#}").contains("failed to create temporary file"),
      "unexpected error: {error:#}"
    );
    assert!(
      fs::symlink_metadata(world.home.join("links/skill-one"))
        .unwrap()
        .file_type()
        .is_symlink(),
      "the unlinked target must be restored by rollback"
    );
    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.contains("[skills.skill-one]"), "metadata unchanged");
  }

  #[test]
  fn stale_lock_state_blocks_removal() {
    let world = fixture();
    fs::write(world.paths().lockfile, "version = 1\n").unwrap();

    let error = execute(&world.offline_env(), "skill-one").unwrap_err();
    assert!(error.to_string().contains("run `skillpm update`"));
  }
}
