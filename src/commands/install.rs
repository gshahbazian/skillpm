use std::fmt;

use anyhow::{Result, bail};

use super::CommandEnv;
use crate::config::ConfigDocument;
use crate::lockfile;
use crate::output;
use crate::store::{SnapshotStatus, Store};
use crate::targets::{self, InstallAction};
use crate::transaction::Transaction;

pub fn run() -> Result<()> {
  let env = CommandEnv::from_process()?;
  let summary = execute(&env)?;
  output::success(&summary.to_string());
  Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InstallSummary {
  pub skills: usize,
  pub created: usize,
  pub repaired: usize,
  pub unchanged: usize,
  pub reconstructed: usize,
}

impl fmt::Display for InstallSummary {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "installed {} skill(s): {} link(s) created, {} repaired, {} unchanged ({} snapshot(s) reconstructed)",
      self.skills, self.created, self.repaired, self.unchanged, self.reconstructed
    )
  }
}

/// Reproduces skillpm.lock exactly: no version resolution, no metadata writes,
/// and no network unless a locked snapshot is missing or corrupt.
pub(crate) fn execute(env: &CommandEnv) -> Result<InstallSummary> {
  let paths = &env.paths;
  let _lock = super::acquire_lock(paths)?;

  let config_doc = ConfigDocument::load(&paths.config_file)?;
  let config = config_doc.config()?;
  let lock_doc = lockfile::require_fresh(&paths.lockfile, &config)?;

  let protected = vec![paths.config_dir().to_path_buf(), paths.data_root.clone()];
  let targets = targets::plan_targets(&config, &paths.home, &protected)?;

  // verify every referenced snapshot; rebuild only what is missing/corrupt
  let store = Store::new(&paths.store);
  let mut reconstructed = 0;
  for (name, entry) in &lock_doc.lockfile.skills {
    match store.verify_snapshot(&entry.content_hash)? {
      SnapshotStatus::Valid => {}
      status => {
        output::progress(&format!(
          "snapshot for '{name}' is {}; reconstructing from the locked source",
          if status == SnapshotStatus::Corrupt {
            "corrupt"
          } else {
            "missing"
          }
        ));
        super::reconstruct_locked(env, &store, name, entry)?;
        reconstructed += 1;
      }
    }

    super::validate_snapshot_identity(&store, name, &entry.content_hash)?;
  }

  let mut transaction = Transaction::new();
  let mut summary = InstallSummary {
    skills: config.skills.len(),
    created: 0,
    repaired: 0,
    unchanged: 0,
    reconstructed,
  };
  for target in &targets {
    let entry = lock_doc
      .lockfile
      .skills
      .get(&target.skill)
      .expect("a fresh lock covers every configured skill");
    let destination = store.snapshot_path(&entry.content_hash)?;
    match targets::stage_install(&mut transaction, target, &destination)? {
      InstallAction::Create => summary.created += 1,
      InstallAction::Replace => summary.repaired += 1,
      InstallAction::Noop => summary.unchanged += 1,
    }
  }

  // re-read metadata bytes immediately before committing (README section 10)
  if config_doc.externally_modified()? {
    bail!("skillpm.toml changed while skillpm was running; aborting without changes");
  }
  if lock_doc.externally_modified()? {
    bail!("skillpm.lock changed while skillpm was running; aborting without changes");
  }

  transaction.commit()?;
  Ok(summary)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::github::{GitHubSkillRequest, prepare_github_skills};
  use crate::local::prepare_local_skill;
  use crate::lockfile::{LockedSkill, Lockfile};
  use crate::source::{GitHubSource, LocalSource};
  use crate::testutil::{self, World, make_remote, write_skill_md};
  use std::fs;

  // shadows super::execute: absorbs transient test-only lock contention
  fn execute(env: &CommandEnv) -> Result<InstallSummary> {
    testutil::retry_lock(|| super::execute(env))
  }
  use std::path::PathBuf;

  const CONFIG: &str = r#"version = 1

[skills.local-skill]
source = "skills/local-skill"
targets = ["links/local-skill"]

[skills.gh-skill]
source = "github:owner/repo/skills/gh-skill"
ref = "main"
targets = ["links/gh-skill", "links2/gh-skill"]
"#;

  struct Fixture {
    world: World,
    remote: testutil::Remote,
    commit: String,
    gh_hash: String,
    local_hash: String,
  }

  /// A fully locked world: config + lock written, both snapshots in the store.
  fn fixture() -> Fixture {
    let world = testutil::world();
    world.create_runtime_dirs();

    // local skill source and snapshot
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    let local = prepare_local_skill(
      &world.store(),
      &LocalSource {
        path: PathBuf::from("skills/local-skill"),
      },
      &world.home,
    )
    .unwrap();

    // github remote, resolved and snapshotted at its current head
    let remote = make_remote(&world, "owner", "repo", &["gh-skill"]);
    let prepared = prepare_github_skills(
      &world.git_env().git,
      &world.store(),
      &[GitHubSkillRequest {
        key: "gh-skill".into(),
        source: GitHubSource {
          owner: "owner".into(),
          repo: "repo".into(),
          path: Some("skills/gh-skill".into()),
        },
        r#ref: Some("main".into()),
        locked: None,
      }],
    )
    .unwrap();

    world.write_config(CONFIG);
    let mut lock = Lockfile::empty();
    lock.skills.insert(
      "local-skill".into(),
      LockedSkill {
        source: "skills/local-skill".into(),
        r#ref: None,
        commit: None,
        content_hash: local.content_hash.clone(),
      },
    );
    lock.skills.insert(
      "gh-skill".into(),
      LockedSkill {
        source: "github:owner/repo/skills/gh-skill".into(),
        r#ref: Some("main".into()),
        commit: Some(prepared[0].commit.clone()),
        content_hash: prepared[0].content_hash.clone(),
      },
    );
    lockfile::write_atomic(&world.paths().lockfile, &lock).unwrap();

    Fixture {
      commit: prepared[0].commit.clone(),
      gh_hash: prepared[0].content_hash.clone(),
      local_hash: local.content_hash,
      world,
      remote,
    }
  }

  fn link_dest(fixture: &Fixture, link: &str) -> PathBuf {
    fs::read_link(fixture.world.home.join(link)).unwrap()
  }

  #[test]
  fn populated_store_installs_offline() {
    let fixture = fixture();
    // the offline env's git binary does not exist: any git spawn would fail
    let summary = execute(&fixture.world.offline_env()).unwrap();

    assert_eq!(
      summary,
      InstallSummary {
        skills: 2,
        created: 3,
        repaired: 0,
        unchanged: 0,
        reconstructed: 0,
      }
    );

    let store = fixture.world.store();
    assert_eq!(
      link_dest(&fixture, "links/gh-skill"),
      store.snapshot_path(&fixture.gh_hash).unwrap()
    );
    assert_eq!(
      link_dest(&fixture, "links2/gh-skill"),
      store.snapshot_path(&fixture.gh_hash).unwrap()
    );
    assert_eq!(
      link_dest(&fixture, "links/local-skill"),
      store.snapshot_path(&fixture.local_hash).unwrap()
    );
  }

  #[test]
  fn install_is_idempotent_and_never_writes_metadata() {
    let fixture = fixture();
    execute(&fixture.world.offline_env()).unwrap();

    let config_before = fixture.world.config_bytes();
    let lock_before = fixture.world.lock_bytes();

    let summary = execute(&fixture.world.offline_env()).unwrap();
    assert_eq!(
      summary,
      InstallSummary {
        skills: 2,
        created: 0,
        repaired: 0,
        unchanged: 3,
        reconstructed: 0,
      }
    );

    assert_eq!(fixture.world.config_bytes(), config_before);
    assert_eq!(fixture.world.lock_bytes(), lock_before);
  }

  #[test]
  fn github_snapshots_reconstruct_at_the_exact_locked_commit() {
    let fixture = fixture();
    let store = fixture.world.store();

    // wipe the cache AND advance the remote; install must not follow it
    store.remove_snapshot(&fixture.gh_hash).unwrap();
    fixture
      .remote
      .push_new_commit("skills/gh-skill/new-file.md", "added later\n");

    let summary = execute(&fixture.world.git_env()).unwrap();
    assert_eq!(summary.reconstructed, 1);
    assert_eq!(
      store.verify_snapshot(&fixture.gh_hash).unwrap(),
      SnapshotStatus::Valid
    );

    let snapshot = store.snapshot_path(&fixture.gh_hash).unwrap();
    assert!(
      !snapshot.join("new-file.md").exists(),
      "reconstruction must use the locked commit {}, not the advanced branch",
      fixture.commit
    );
  }

  #[test]
  fn local_snapshots_reconstruct_only_on_exact_source_match() {
    let fixture = fixture();
    let store = fixture.world.store();

    // unchanged source: reconstruction succeeds offline
    store.remove_snapshot(&fixture.local_hash).unwrap();
    let summary = execute(&fixture.world.offline_env()).unwrap();
    assert_eq!(summary.reconstructed, 1);
    assert_eq!(
      store.verify_snapshot(&fixture.local_hash).unwrap(),
      SnapshotStatus::Valid
    );

    // drifted source: reconstruction is refused and nothing else changes
    store.remove_snapshot(&fixture.local_hash).unwrap();
    fs::write(
      fixture.world.home.join("skills/local-skill/drift.md"),
      "changed\n",
    )
    .unwrap();
    let error = execute(&fixture.world.offline_env()).unwrap_err();
    assert!(format!("{error:#}").contains("no longer matches"));
  }

  #[test]
  fn stale_lock_state_is_rejected() {
    let fixture = fixture();
    fixture
      .world
      .write_config(&CONFIG.replace("ref = \"main\"", "ref = \"other\""));

    let error = execute(&fixture.world.offline_env()).unwrap_err();
    assert!(error.to_string().contains("run `skillpm update`"));
  }

  #[test]
  #[cfg(unix)]
  fn corrupt_snapshots_are_repaired() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let store = fixture.world.store();
    let snapshot = store.snapshot_path(&fixture.gh_hash).unwrap();

    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(snapshot.join("SKILL.md"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(snapshot.join("SKILL.md"), "tampered").unwrap();

    let summary = execute(&fixture.world.git_env()).unwrap();
    assert_eq!(summary.reconstructed, 1);
    assert_eq!(
      store.verify_snapshot(&fixture.gh_hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  fn install_rejects_a_snapshot_whose_name_does_not_match_its_config_key() {
    let fixture = fixture();

    let config = String::from_utf8(fixture.world.config_bytes())
      .unwrap()
      .replace("[skills.local-skill]", "[skills.alias]")
      .replace("links/local-skill", "links/alias");
    fixture.world.write_config(&config);

    let lock = String::from_utf8(fixture.world.lock_bytes())
      .unwrap()
      .replace("[skills.local-skill]", "[skills.alias]");
    fs::write(&fixture.world.paths().lockfile, lock).unwrap();

    let error = execute(&fixture.world.offline_env()).unwrap_err();
    assert!(
      error
        .to_string()
        .contains("config key 'alias' does not match skill name 'local-skill'")
    );
    assert!(!fixture.world.home.join("links/alias").exists());
  }

  #[test]
  fn a_regular_file_at_any_target_aborts_everything() {
    let fixture = fixture();
    let occupied = fixture.world.home.join("links2/gh-skill");
    fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    fs::write(&occupied, "user data").unwrap();

    let error = execute(&fixture.world.offline_env()).unwrap_err();
    assert!(error.to_string().contains("refusing to replace"));

    // no other target was touched
    assert!(!fixture.world.home.join("links/gh-skill").exists());
    assert!(!fixture.world.home.join("links/local-skill").exists());
    assert_eq!(fs::read_to_string(&occupied).unwrap(), "user data");
  }

  #[test]
  fn install_never_touches_source_directories() {
    let fixture = fixture();
    execute(&fixture.world.offline_env()).unwrap();

    let names: Vec<_> = fs::read_dir(fixture.world.home.join("skills/local-skill"))
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("SKILL.md")]);
  }

  #[test]
  fn a_deleted_data_root_is_rebuilt_from_config_and_lock() {
    let fixture = fixture();

    // simulate a synced-dotfiles machine / deleted cache: config and lock
    // exist, the entire data root does not
    let store = fixture.world.store();
    store.remove_snapshot(&fixture.gh_hash).unwrap();
    store.remove_snapshot(&fixture.local_hash).unwrap();
    fs::remove_dir_all(fixture.world.paths().data_root).unwrap();

    let summary = execute(&fixture.world.git_env()).unwrap();
    assert_eq!(summary.reconstructed, 2);
    assert_eq!(summary.created, 3);
    assert_eq!(
      store.verify_snapshot(&fixture.gh_hash).unwrap(),
      SnapshotStatus::Valid
    );
    assert_eq!(
      store.verify_snapshot(&fixture.local_hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  fn an_unbootstrapped_machine_gets_a_clear_error() {
    let world = testutil::world();
    let error = execute(&world.offline_env()).unwrap_err();
    assert!(error.to_string().contains("run `skillpm add`"));
  }
}
