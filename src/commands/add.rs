use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::CommandEnv;
use crate::config::{ConfigDocument, Skill};
use crate::github::{self, GitHubSkillRequest};
use crate::local;
use crate::lockfile::{self, LockedSkill, Lockfile, LockfileDocument};
use crate::output;
use crate::paths::{OperationLock, resolve_user_path};
use crate::source::{Source, parse_source};
use crate::store::{SnapshotStatus, Store};
use crate::targets::{self, InstallAction};
use crate::transaction::{ExpectedFile, Transaction};

pub fn run(source: String, targets: Vec<PathBuf>, r#ref: Option<String>) -> Result<()> {
  let env = CommandEnv::from_process()?;
  let summary = execute(&env, &source, &targets, r#ref.as_deref())?;
  output::success(&summary.to_string());
  Ok(())
}

#[derive(Debug, PartialEq)]
pub(crate) struct AddSummary {
  pub name: String,
  pub already_configured: bool,
  pub targets_added: usize,
  pub created: usize,
  pub repaired: usize,
  pub unchanged: usize,
}

impl fmt::Display for AddSummary {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if self.already_configured {
      write!(
        f,
        "skill '{}' was already configured: {} target(s) added, {} link(s) created, {} repaired, {} verified",
        self.name, self.targets_added, self.created, self.repaired, self.unchanged
      )
    } else {
      write!(
        f,
        "added skill '{}': {} link(s) created, {} repaired, {} verified",
        self.name, self.created, self.repaired, self.unchanged
      )
    }
  }
}

pub(crate) fn execute(
  env: &CommandEnv,
  source_str: &str,
  new_targets: &[PathBuf],
  r#ref: Option<&str>,
) -> Result<AddSummary> {
  execute_with_hook(env, source_str, new_targets, r#ref, &mut || Ok(()))
}

/// The hook runs right before the transaction commits; tests use it to
/// simulate concurrent external edits.
fn execute_with_hook(
  env: &CommandEnv,
  source_str: &str,
  new_targets: &[PathBuf],
  r#ref: Option<&str>,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<AddSummary> {
  let paths = &env.paths;
  if new_targets.is_empty() {
    bail!("at least one --target is required");
  }

  let source = parse_source(source_str)?;
  source.validate_ref(r#ref)?;

  // add is the only command that bootstraps absent directories
  paths.create_runtime_dirs()?;
  let _lock = OperationLock::acquire(&paths.operation_lock)?;

  let config_existed = paths.config_file.exists();
  let mut config_doc = ConfigDocument::load_or_empty(&paths.config_file)?;
  let config = config_doc.config()?;

  // once a config exists, add requires fresh complete lock state
  let lock_doc: Option<LockfileDocument> = if config_existed {
    Some(lockfile::require_fresh(&paths.lockfile, &config)?)
  } else {
    if paths.lockfile.exists() {
      bail!(
        "found {} without a config; delete the stale lockfile and retry",
        paths.lockfile.display()
      );
    }
    None
  };
  let old_lock = lock_doc
    .as_ref()
    .map(|doc| doc.lockfile.clone())
    .unwrap_or_else(Lockfile::empty);

  let store = Store::new(&paths.store);
  let mut created_snapshots: Vec<String> = Vec::new();
  let result = stage_and_commit(
    env,
    &store,
    &mut config_doc,
    lock_doc.as_ref(),
    &old_lock,
    &source,
    source_str,
    new_targets,
    r#ref,
    &mut created_snapshots,
    before_commit,
  );

  match result {
    Ok(committed) => {
      let referenced: BTreeSet<String> = committed.referenced.iter().cloned().collect();
      for warning in store.prune(&referenced) {
        output::warning(&warning);
      }
      Ok(committed.summary)
    }
    Err(error) => {
      // a snapshot created by this failed run is not referenced by any lock
      for hash in &created_snapshots {
        let _ = store.remove_snapshot(hash);
      }
      Err(error)
    }
  }
}

struct CommittedAdd {
  summary: AddSummary,
  referenced: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn stage_and_commit(
  env: &CommandEnv,
  store: &Store,
  config_doc: &mut ConfigDocument,
  lock_doc: Option<&LockfileDocument>,
  old_lock: &Lockfile,
  source: &Source,
  source_str: &str,
  new_targets: &[PathBuf],
  r#ref: Option<&str>,
  created_snapshots: &mut Vec<String>,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<CommittedAdd> {
  let paths = &env.paths;
  let config = config_doc.config()?;

  // an entry with the identical source/ref keeps its locked version; add
  // never performs a selective update
  let existing = config
    .skills
    .iter()
    .find(|(_, skill)| skill.source == source_str && skill.r#ref.as_deref() == r#ref)
    .map(|(name, skill)| (name.clone(), skill.clone()));

  let (name, entry, added_targets, already_configured) = match existing {
    Some((name, existing_skill)) => {
      let entry = old_lock
        .skills
        .get(&name)
        .cloned()
        .expect("fresh lock state covers every configured skill");

      // install check: the locked snapshot must exist and verify
      if store.verify_snapshot(&entry.content_hash)? != SnapshotStatus::Valid {
        output::progress(&format!(
          "snapshot for '{name}' is missing or corrupt; reconstructing from the locked source"
        ));
        super::reconstruct_locked(env, store, &name, &entry)?;
      }

      let added = new_target_paths(&existing_skill.targets, new_targets, &paths.home)?;
      (name, entry, added, true)
    }
    None => {
      // resolve and validate the new source before touching any metadata
      let (skill_name, content_hash, commit, created) =
        prepare_new_source(env, store, source, source_str, r#ref)?;
      if created {
        created_snapshots.push(content_hash.clone());
      }

      // a same-name skill from a different source/ref requires explicit removal
      if config.skills.contains_key(&skill_name) {
        bail!(
          "skill '{skill_name}' is already configured from a different source or ref; run `skillpm remove {skill_name}` first"
        );
      }

      let entry = LockedSkill {
        source: source_str.to_string(),
        r#ref: r#ref.map(String::from),
        commit,
        content_hash,
      };
      let deduped = new_target_paths(&[], new_targets, &paths.home)?;
      (skill_name, entry, deduped, false)
    }
  };
  let targets_added = added_targets.len();

  // an identical invocation edits nothing; a merge appends only the new
  // array elements. Either way the user-authored formatting survives.
  let config_changed = targets_added > 0 || !already_configured;
  if already_configured {
    if !added_targets.is_empty() {
      config_doc.append_targets(&name, &added_targets)?;
    }
  } else {
    config_doc.upsert_skill(
      &name,
      &Skill {
        source: source_str.to_string(),
        r#ref: r#ref.map(String::from),
        targets: added_targets,
      },
    )?;
  }

  let updated_config = config_doc.config()?;
  let protected = vec![paths.config_dir().to_path_buf(), paths.data_root.clone()];
  let plan = targets::plan_targets(&updated_config, &paths.home, &protected)?;

  let mut new_lock = old_lock.clone();
  new_lock.skills.insert(name.clone(), entry);
  let rendered_lock = lockfile::render_validated(&new_lock)?;
  let lock_original = lock_doc.and_then(|doc| doc.original_bytes());
  let lock_changed = lock_original != Some(rendered_lock.as_slice());

  let mut transaction = Transaction::new();
  if config_changed {
    let expected = match config_doc.original_bytes() {
      Some(bytes) => ExpectedFile::Bytes(bytes.to_vec()),
      None => ExpectedFile::Absent,
    };
    transaction.write_file(&paths.config_file, config_doc.rendered_bytes()?, expected);
  }
  if lock_changed {
    let expected = match lock_original {
      Some(bytes) => ExpectedFile::Bytes(bytes.to_vec()),
      None => ExpectedFile::Absent,
    };
    transaction.write_file(&paths.lockfile, rendered_lock, expected);
  }

  let mut summary = AddSummary {
    name: name.clone(),
    already_configured,
    targets_added,
    created: 0,
    repaired: 0,
    unchanged: 0,
  };
  let destination = store.snapshot_path(&new_lock.skills[&name].content_hash)?;
  for target in plan.iter().filter(|target| target.skill == name) {
    match targets::stage_install(&mut transaction, target, &destination)? {
      InstallAction::Create => summary.created += 1,
      InstallAction::Replace => summary.repaired += 1,
      InstallAction::Noop => summary.unchanged += 1,
    }
  }

  before_commit()?;

  // re-read metadata bytes immediately before committing (README section 10);
  // these also cover the skip-unchanged-write paths
  if config_doc.externally_modified()? {
    bail!("skillpm.toml changed while skillpm was running; aborting without changes");
  }
  if let Some(doc) = lock_doc
    && doc.externally_modified()?
  {
    bail!("skillpm.lock changed while skillpm was running; aborting without changes");
  }

  transaction.commit()?;

  let referenced = new_lock
    .skills
    .values()
    .map(|entry| entry.content_hash.clone())
    .collect();
  Ok(CommittedAdd {
    summary,
    referenced,
  })
}

/// Fetches, validates, and snapshots a brand-new source, returning
/// (name, content_hash, commit, created).
fn prepare_new_source(
  env: &CommandEnv,
  store: &Store,
  source: &Source,
  source_str: &str,
  r#ref: Option<&str>,
) -> Result<(String, String, Option<String>, bool)> {
  match source {
    Source::Local(local_source) => {
      let prepared = local::prepare_local_skill(store, local_source, &env.paths.home)?;
      Ok((prepared.name, prepared.content_hash, None, prepared.created))
    }
    Source::GitHub(github_source) => {
      let prepared = github::prepare_github_skills(
        &env.git,
        store,
        &[GitHubSkillRequest {
          key: source_str.to_string(),
          source: github_source.clone(),
          r#ref: r#ref.map(String::from),
          locked: None,
        }],
      )?
      .pop()
      .context("GitHub preparation returned nothing")?;
      Ok((
        prepared.name,
        prepared.content_hash,
        Some(prepared.commit),
        prepared.created,
      ))
    }
  }
}

/// Returns only the genuinely new targets, deduplicated with the same
/// canonical identity target planning uses — so a symlink-aliased spelling
/// of an existing target merges away instead of tripping the duplicate check.
fn new_target_paths(
  existing: &[PathBuf],
  incoming: &[PathBuf],
  home: &Path,
) -> Result<Vec<PathBuf>> {
  let identity = |target: &PathBuf| -> Result<PathBuf> {
    targets::canonical_target(&resolve_user_path(target, home)?)
  };

  let mut seen: Vec<PathBuf> = existing.iter().map(identity).collect::<Result<_>>()?;

  let mut added = Vec::new();
  for target in incoming {
    let candidate = identity(target)?;
    if !seen.contains(&candidate) {
      seen.push(candidate);
      added.push(target.clone());
    }
  }
  Ok(added)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::{self, World, make_remote, write_skill_md};
  use std::fs;

  // shadows super::execute: absorbs transient test-only lock contention
  fn execute(
    env: &CommandEnv,
    source: &str,
    targets: &[PathBuf],
    r#ref: Option<&str>,
  ) -> Result<AddSummary> {
    testutil::retry_lock(|| super::execute(env, source, targets, r#ref))
  }

  fn add_local(world: &World) -> Result<AddSummary> {
    execute(
      &world.offline_env(),
      "skills/local-skill",
      &[PathBuf::from("links/local-skill")],
      None,
    )
  }

  #[test]
  fn first_add_bootstraps_everything() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");

    // nothing exists yet: no config dir, no data root
    let summary = add_local(&world).unwrap();
    assert_eq!(
      summary,
      AddSummary {
        name: "local-skill".into(),
        already_configured: false,
        targets_added: 1,
        created: 1,
        repaired: 0,
        unchanged: 0,
      }
    );

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.starts_with("version = 1\n"));
    assert!(config.contains("[skills.local-skill]"));
    assert!(config.contains("source = \"skills/local-skill\""));

    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    assert!(lock.contains("[skills.local-skill]"));
    assert!(lock.contains("content_hash = \"sha256:"));

    assert!(
      fs::read_link(world.home.join("links/local-skill"))
        .unwrap()
        .exists()
    );
  }

  #[test]
  fn identical_repeat_is_an_idempotent_install_check() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    add_local(&world).unwrap();

    let config_before = world.config_bytes();
    let lock_before = world.lock_bytes();

    let summary = add_local(&world).unwrap();
    assert!(summary.already_configured);
    assert_eq!(summary.targets_added, 0);
    assert_eq!(
      summary.unchanged, 1,
      "the correct link is verified, not rewritten"
    );

    assert_eq!(
      world.config_bytes(),
      config_before,
      "config untouched byte for byte"
    );
    assert_eq!(world.lock_bytes(), lock_before);
  }

  #[test]
  fn github_add_with_ref_locks_and_installs() {
    let world = testutil::world();
    let remote = make_remote(&world, "owner", "repo", &["gh-skill"]);

    let summary = execute(
      &world.git_env(),
      "github:owner/repo/skills/gh-skill",
      &[PathBuf::from("links/gh-skill")],
      Some("main"),
    )
    .unwrap();
    assert_eq!(summary.name, "gh-skill");
    assert_eq!(summary.created, 1);

    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    assert!(lock.contains(&format!("commit = \"{}\"", remote.head_sha())));
    assert!(lock.contains("ref = \"main\""));
  }

  #[test]
  fn refs_on_local_sources_are_rejected() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");

    let error = execute(
      &world.offline_env(),
      "skills/local-skill",
      &[PathBuf::from("links/local-skill")],
      Some("main"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("only valid for GitHub"));
    assert!(
      !world.paths().config_file.exists(),
      "nothing may be written"
    );
  }

  #[test]
  fn same_source_merges_targets_without_updating_the_version() {
    let world = testutil::world();
    let remote = make_remote(&world, "owner", "repo", &["gh-skill"]);
    let env = world.git_env();
    let source = "github:owner/repo/skills/gh-skill";

    execute(
      &env,
      source,
      &[PathBuf::from("links/gh-skill")],
      Some("main"),
    )
    .unwrap();
    let locked = remote.head_sha();

    // the branch moves on; adding a new target must NOT advance the lock
    remote.push_new_commit("skills/gh-skill/later.md", "later\n");

    let summary = execute(
      &env,
      source,
      &[
        PathBuf::from("links2/gh-skill"),
        PathBuf::from("links/gh-skill"),
      ],
      Some("main"),
    )
    .unwrap();
    assert!(summary.already_configured);
    assert_eq!(
      summary.targets_added, 1,
      "the duplicate target is deduplicated"
    );
    assert_eq!(summary.created, 1);
    assert_eq!(summary.unchanged, 1);

    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    assert!(
      lock.contains(&format!("commit = \"{locked}\"")),
      "add never performs a selective version update"
    );

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.contains("links2/gh-skill"));
  }

  #[test]
  #[cfg(unix)]
  fn symlink_aliased_targets_merge_instead_of_colliding() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    fs::create_dir_all(world.home.join("real")).unwrap();
    std::os::unix::fs::symlink(world.home.join("real"), world.home.join("alias")).unwrap();

    execute(
      &world.offline_env(),
      "skills/local-skill",
      &[PathBuf::from("real/local-skill")],
      None,
    )
    .unwrap();

    // the aliased spelling is the same location; it must dedupe, not error
    let summary = execute(
      &world.offline_env(),
      "skills/local-skill",
      &[PathBuf::from("alias/local-skill")],
      None,
    )
    .unwrap();
    assert!(summary.already_configured);
    assert_eq!(summary.targets_added, 0);
    assert_eq!(summary.unchanged, 1);
  }

  #[test]
  fn merging_preserves_field_comments_and_array_formatting() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    add_local(&world).unwrap();

    // decorate the entry the way a user would
    let decorated = r#"version = 1

[skills.local-skill]
source = "skills/local-skill" # keep this comment
targets = [
  "links/local-skill", # primary
]
"#;
    world.write_config(decorated);

    let summary = execute(
      &world.offline_env(),
      "skills/local-skill",
      &[PathBuf::from("links2/local-skill")],
      None,
    )
    .unwrap();
    assert_eq!(summary.targets_added, 1);

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(
      config.contains("source = \"skills/local-skill\" # keep this comment"),
      "field comment must survive a merge:\n{config}"
    );
    assert!(
      config.contains("\"links/local-skill\", # primary"),
      "existing array element formatting must survive:\n{config}"
    );
    assert!(config.contains("links2/local-skill"));
  }

  #[test]
  fn comments_and_formatting_survive_adding_another_skill() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    write_skill_md(&world.home.join("skills/other-skill"), "other-skill");
    add_local(&world).unwrap();

    // decorate the config the way a user would
    let decorated = format!(
      "# my skills, do not touch\n{}",
      String::from_utf8(world.config_bytes()).unwrap()
    );
    world.write_config(&decorated);
    // the config edit invalidated the lock? no: source/ref unchanged, so
    // lock state is still fresh
    let summary = execute(
      &world.offline_env(),
      "skills/other-skill",
      &[PathBuf::from("links/other-skill")],
      None,
    )
    .unwrap();
    assert_eq!(summary.name, "other-skill");

    let config = String::from_utf8(world.config_bytes()).unwrap();
    assert!(config.starts_with("# my skills, do not touch\n"));
    assert!(config.contains("[skills.local-skill]"));
    assert!(config.contains("[skills.other-skill]"));
  }

  #[test]
  fn name_collisions_require_explicit_removal() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    add_local(&world).unwrap();

    // a different source directory whose SKILL.md declares the same name
    write_skill_md(&world.home.join("elsewhere/local-skill"), "local-skill");
    let error = execute(
      &world.offline_env(),
      "elsewhere/local-skill",
      &[PathBuf::from("links2/local-skill")],
      None,
    )
    .unwrap_err();
    assert!(
      error
        .to_string()
        .contains("run `skillpm remove local-skill`")
    );
  }

  #[test]
  fn stale_lock_state_blocks_adds() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    write_skill_md(&world.home.join("skills/other-skill"), "other-skill");
    add_local(&world).unwrap();

    fs::write(world.paths().lockfile, "version = 1\n").unwrap(); // stale: entry missing

    let error = execute(
      &world.offline_env(),
      "skills/other-skill",
      &[PathBuf::from("links/other-skill")],
      None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("run `skillpm update`"));
  }

  #[test]
  fn invalid_skills_and_targets_change_nothing() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    add_local(&world).unwrap();
    let config_before = world.config_bytes();
    let lock_before = world.lock_bytes();

    // no SKILL.md at the new source
    fs::create_dir_all(world.home.join("skills/broken")).unwrap();
    let error = execute(
      &world.offline_env(),
      "skills/broken",
      &[PathBuf::from("links/broken")],
      None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("no SKILL.md"));

    // basename does not match the skill name
    write_skill_md(&world.home.join("skills/other-skill"), "other-skill");
    let error = execute(
      &world.offline_env(),
      "skills/other-skill",
      &[PathBuf::from("links/wrong-name")],
      None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("must end in the skill name"));

    assert_eq!(world.config_bytes(), config_before);
    assert_eq!(world.lock_bytes(), lock_before);
    // and the failed run's snapshot was cleaned up: only one referenced left
    let snapshots = fs::read_dir(world.paths().store.join("sha256"))
      .unwrap()
      .count();
    assert_eq!(snapshots, 1);
  }

  #[test]
  fn a_blocked_target_rolls_back_config_and_lock_together() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");

    let blocked = world.home.join("links/local-skill");
    fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    fs::write(&blocked, "user data").unwrap();

    let error = add_local(&world).unwrap_err();
    assert!(error.to_string().contains("refusing to replace"));

    assert!(
      !world.paths().config_file.exists(),
      "bootstrap config rolled back"
    );
    assert!(
      !world.paths().lockfile.exists(),
      "bootstrap lock rolled back"
    );
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "user data");
  }

  #[test]
  fn external_config_edits_mid_add_abort() {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    write_skill_md(&world.home.join("skills/other-skill"), "other-skill");
    add_local(&world).unwrap();

    let config_path = world.paths().config_file;
    let env = world.offline_env();
    let error = testutil::retry_lock(|| {
      super::execute_with_hook(
        &env,
        "skills/other-skill",
        &[PathBuf::from("links/other-skill")],
        None,
        &mut || {
          let mut bytes = fs::read(&config_path).unwrap();
          bytes.extend_from_slice(b"\n# edited externally\n");
          fs::write(&config_path, bytes).unwrap();
          Ok(())
        },
      )
    })
    .unwrap_err();

    assert!(error.to_string().contains("skillpm.toml changed"));
  }
}
