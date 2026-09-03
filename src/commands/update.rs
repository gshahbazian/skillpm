use std::collections::BTreeSet;
use std::fmt;

use anyhow::{Result, bail};

use super::CommandEnv;
use crate::config::ConfigDocument;
use crate::github::{self, GitHubSkillRequest, LockedGitHub};
use crate::local;
use crate::lockfile::{self, LockedSkill, Lockfile};
use crate::output;
use crate::skill;
use crate::source::{Source, parse_source};
use crate::store::Store;
use crate::targets::{self, InstallAction};
use crate::transaction::{ExpectedFile, Transaction};

pub fn run() -> Result<()> {
  let env = CommandEnv::from_process()?;
  let summary = execute(&env)?;
  output::success(&summary.to_string());
  Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UpdateSummary {
  pub skills: usize,
  pub changed_skills: Vec<ChangedSkill>,
  pub lock_written: bool,
  pub created: usize,
  pub repaired: usize,
  pub links_unchanged: usize,
  pub prune_warnings: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ChangedSkill {
  name: String,
  checkout_change: Option<CheckoutChange>,
}

#[derive(Debug, PartialEq, Eq)]
struct CheckoutChange {
  previous: Option<String>,
  current: String,
}

impl ChangedSkill {
  fn from_lock_entries(
    name: String,
    previous: Option<&LockedSkill>,
    current: &LockedSkill,
  ) -> Self {
    Self {
      name,
      checkout_change: current.commit.as_ref().map(|checkout| CheckoutChange {
        previous: previous.and_then(|entry| entry.commit.clone()),
        current: checkout.clone(),
      }),
    }
  }
}

impl fmt::Display for ChangedSkill {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.name)?;

    let Some(change) = &self.checkout_change else {
      return Ok(());
    };

    let previous = change
      .previous
      .as_deref()
      .map(short_checkout)
      .unwrap_or("none");
    write!(f, " ({previous} -> {})", short_checkout(&change.current))
  }
}

fn short_checkout(checkout: &str) -> &str {
  checkout.get(..7).unwrap_or(checkout)
}

impl fmt::Display for UpdateSummary {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "updated {} skill(s): {} changed, {} link(s) created, {} repointed, {} unchanged",
      self.skills,
      self.changed_skills.len(),
      self.created,
      self.repaired,
      self.links_unchanged
    )?;

    if self.changed_skills.is_empty() {
      return Ok(());
    }

    write!(f, "\nchanged skills:")?;
    for skill in &self.changed_skills {
      write!(f, "\n- {skill}")?;
    }

    Ok(())
  }
}

/// The only workflow that resolves new versions or repairs lock state:
/// resolve everything, regenerate a complete lockfile, install every target,
/// commit metadata and links as one transaction, then prune.
pub(crate) fn execute(env: &CommandEnv) -> Result<UpdateSummary> {
  execute_with_hook(env, &mut || Ok(()))
}

/// The hook runs right before the transaction commits; tests use it to
/// simulate concurrent external edits.
fn execute_with_hook(
  env: &CommandEnv,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<UpdateSummary> {
  let paths = &env.paths;
  let _lock = super::acquire_lock(paths)?;

  let config_doc = ConfigDocument::load(&paths.config_file)?;
  let config = config_doc.config()?;

  // missing/malformed/older/stale lock state regenerates; newer refuses
  let update_lock = lockfile::load_for_update(&paths.lockfile)?;

  let store = Store::new(&paths.store);
  let mut created_snapshots: Vec<String> = Vec::new();
  let result = prepare_and_commit(
    env,
    &store,
    &config_doc,
    &config,
    &update_lock,
    &mut created_snapshots,
    before_commit,
  );

  let mut summary = match result {
    Ok(summary) => summary,
    Err(error) => {
      // snapshots created by this failed run cannot be referenced by the old
      // lock (they did not exist before it), so removing them is safe
      for hash in &created_snapshots {
        let _ = store.remove_snapshot(hash);
      }
      return Err(error);
    }
  };

  // prune failures warn but never fail an otherwise successful update
  let referenced: BTreeSet<String> = summary.referenced.iter().cloned().collect();
  let warnings = store.prune(&referenced);
  summary.summary.prune_warnings = warnings.len();
  for warning in &warnings {
    output::warning(warning);
  }

  Ok(summary.summary)
}

struct CommittedUpdate {
  summary: UpdateSummary,
  referenced: Vec<String>,
}

fn prepare_and_commit(
  env: &CommandEnv,
  store: &Store,
  config_doc: &ConfigDocument,
  config: &crate::config::Config,
  update_lock: &lockfile::UpdateLockDocument,
  created_snapshots: &mut Vec<String>,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<CommittedUpdate> {
  let paths = &env.paths;
  let reusable = update_lock.reusable.clone().unwrap_or_else(Lockfile::empty);

  let protected = vec![paths.config_dir().to_path_buf(), paths.data_root.clone()];
  let targets = targets::plan_targets(config, &paths.home, &protected)?;

  let mut new_lock = Lockfile::empty();
  let mut github_requests = Vec::new();

  for (name, skill) in &config.skills {
    let source = parse_source(&skill.source)?;
    source.validate_ref(skill.r#ref.as_deref())?;

    match source {
      Source::GitHub(source) => {
        // the locked commit/hash feed the unchanged fast path, but only when
        // the lock entry still mirrors the configured source and ref
        let locked = reusable
          .skills
          .get(name)
          .filter(|entry| entry.source == skill.source && entry.r#ref == skill.r#ref)
          .and_then(|entry| {
            entry.commit.clone().map(|commit| LockedGitHub {
              commit,
              content_hash: entry.content_hash.clone(),
            })
          });
        github_requests.push(GitHubSkillRequest {
          key: name.clone(),
          source,
          r#ref: skill.r#ref.clone(),
          locked,
        });
      }
      Source::Local(local_source) => {
        let prepared = local::prepare_local_skill(store, &local_source, &paths.home)?;
        if prepared.created {
          created_snapshots.push(prepared.content_hash.clone());
        }
        skill::ensure_key_matches(name, &prepared.name)?;
        new_lock.skills.insert(
          name.clone(),
          LockedSkill {
            source: skill.source.clone(),
            r#ref: None,
            commit: None,
            content_hash: prepared.content_hash,
          },
        );
      }
    }
  }

  for prepared in github::prepare_github_skills(&env.git, store, &github_requests)? {
    if prepared.created {
      created_snapshots.push(prepared.content_hash.clone());
    }
    skill::ensure_key_matches(&prepared.key, &prepared.name)?;
    let skill = &config.skills[&prepared.key];
    new_lock.skills.insert(
      prepared.key.clone(),
      LockedSkill {
        source: skill.source.clone(),
        r#ref: skill.r#ref.clone(),
        commit: Some(prepared.commit),
        content_hash: prepared.content_hash,
      },
    );
  }

  let changed_skills = new_lock
    .skills
    .iter()
    .filter_map(|(name, current)| {
      let previous = reusable.skills.get(name.as_str());
      if previous == Some(current) {
        return None;
      }

      Some(ChangedSkill::from_lock_entries(
        name.clone(),
        previous,
        current,
      ))
    })
    .collect();

  // an unchanged lockfile is not rewritten
  let rendered = lockfile::render_validated(&new_lock)?;
  let lock_written = update_lock.original_bytes() != Some(rendered.as_slice());

  let mut transaction = Transaction::new();
  if lock_written {
    let expected = match update_lock.original_bytes() {
      Some(bytes) => ExpectedFile::Bytes(bytes.to_vec()),
      None => ExpectedFile::Absent,
    };
    transaction.write_file(&paths.lockfile, rendered, expected);
  }

  let mut summary = UpdateSummary {
    skills: config.skills.len(),
    changed_skills,
    lock_written,
    created: 0,
    repaired: 0,
    links_unchanged: 0,
    prune_warnings: 0,
  };
  for target in &targets {
    let entry = new_lock
      .skills
      .get(&target.skill)
      .expect("every configured skill was prepared");
    let destination = store.snapshot_path(&entry.content_hash)?;
    match targets::stage_install(&mut transaction, target, &destination)? {
      InstallAction::Create => summary.created += 1,
      InstallAction::Replace => summary.repaired += 1,
      InstallAction::Noop => summary.links_unchanged += 1,
    }
  }

  before_commit()?;

  // re-read config AND lock bytes immediately before committing (README
  // section 10); the lock check must run even when the lockfile is unchanged
  // and therefore not part of the transaction
  if config_doc.externally_modified()? {
    bail!("skillpm.toml changed while skillpm was running; aborting without changes");
  }
  if update_lock.externally_modified()? {
    bail!("skillpm.lock changed while skillpm was running; aborting without changes");
  }

  transaction.commit()?;

  let referenced = new_lock
    .skills
    .values()
    .map(|entry| entry.content_hash.clone())
    .collect();
  Ok(CommittedUpdate {
    summary,
    referenced,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::{self, World, git, make_remote, write_skill_md};
  use std::fs;
  use std::path::PathBuf;

  // shadows super::execute: absorbs transient test-only lock contention
  fn execute(env: &CommandEnv) -> Result<UpdateSummary> {
    testutil::retry_lock(|| super::execute(env))
  }

  const CONFIG: &str = r#"version = 1

[skills.local-skill]
source = "skills/local-skill"
targets = ["links/local-skill"]

[skills.gh-skill]
source = "github:owner/repo/skills/gh-skill"
ref = "main"
targets = ["links/gh-skill"]
"#;

  struct Fixture {
    world: World,
    remote: testutil::Remote,
  }

  fn fixture() -> Fixture {
    let world = testutil::world();
    write_skill_md(&world.home.join("skills/local-skill"), "local-skill");
    let remote = make_remote(&world, "owner", "repo", &["gh-skill"]);
    world.write_config(CONFIG);
    Fixture { world, remote }
  }

  fn locked_commit(world: &World, name: &str) -> String {
    let lock = String::from_utf8(world.lock_bytes()).unwrap();
    let mut in_entry = false;
    for line in lock.lines() {
      if line == format!("[skills.{name}]") {
        in_entry = true;
      } else if line.starts_with("[skills.") {
        in_entry = false;
      } else if in_entry && let Some(commit) = line.strip_prefix("commit = \"") {
        return commit.trim_end_matches('"').to_string();
      }
    }
    panic!("no commit for {name} in lock:\n{lock}");
  }

  fn link_dest(world: &World, link: &str) -> PathBuf {
    fs::read_link(world.home.join(link)).unwrap()
  }

  #[cfg(unix)]
  fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
  }

  fn changed_names(summary: &UpdateSummary) -> Vec<&str> {
    summary
      .changed_skills
      .iter()
      .map(|skill| skill.name.as_str())
      .collect()
  }

  #[test]
  fn summary_lists_changed_skills_after_the_main_line() {
    let summary = UpdateSummary {
      skills: 3,
      changed_skills: vec![
        ChangedSkill {
          name: "gh-skill".to_string(),
          checkout_change: Some(CheckoutChange {
            previous: Some("a1b2c3d000000000000000000000000000000000".to_string()),
            current: "d4e5f6a000000000000000000000000000000000".to_string(),
          }),
        },
        ChangedSkill {
          name: "local-skill".to_string(),
          checkout_change: None,
        },
      ],
      lock_written: true,
      created: 0,
      repaired: 2,
      links_unchanged: 1,
      prune_warnings: 0,
    };

    assert_eq!(
      summary.to_string(),
      concat!(
        "updated 3 skill(s): 2 changed, 0 link(s) created, 2 repointed, 1 unchanged\n",
        "changed skills:\n",
        "- gh-skill (a1b2c3d -> d4e5f6a)\n",
        "- local-skill",
      )
    );
  }

  #[test]
  fn bootstraps_missing_lock_and_is_idempotent() {
    let fixture = fixture();
    let env = fixture.world.git_env();

    let first = execute(&env).unwrap();
    assert_eq!(first.skills, 2);
    assert_eq!(changed_names(&first), ["gh-skill", "local-skill"]);
    let checkout_change = first.changed_skills[0].checkout_change.as_ref().unwrap();
    assert_eq!(checkout_change.previous, None);
    assert_eq!(checkout_change.current, fixture.remote.head_sha());
    assert!(first.to_string().contains("gh-skill (none -> "));
    assert!(first.lock_written);
    assert_eq!(first.created, 2);

    // lock, store, and targets agree on one version set
    assert_eq!(
      locked_commit(&fixture.world, "gh-skill"),
      fixture.remote.head_sha()
    );
    assert!(link_dest(&fixture.world, "links/gh-skill").exists());
    assert!(link_dest(&fixture.world, "links/local-skill").exists());

    // idempotence: nothing changes, and the unchanged lockfile is not
    // rewritten (same inode = no rename over it)
    #[cfg(unix)]
    let lock_inode = inode(&fixture.world.paths().lockfile);
    let second = execute(&env).unwrap();
    assert_eq!(
      second,
      UpdateSummary {
        skills: 2,
        changed_skills: Vec::new(),
        lock_written: false,
        created: 0,
        repaired: 0,
        links_unchanged: 2,
        prune_warnings: 0,
      }
    );
    #[cfg(unix)]
    assert_eq!(inode(&fixture.world.paths().lockfile), lock_inode);
  }

  #[test]
  fn moved_branches_repoint_targets_and_prune_old_snapshots() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();

    let old_dest = link_dest(&fixture.world, "links/gh-skill");
    let old_head = locked_commit(&fixture.world, "gh-skill");
    let new_head = fixture
      .remote
      .push_new_commit("skills/gh-skill/extra.md", "new content\n");

    let summary = execute(&env).unwrap();
    assert_eq!(changed_names(&summary), ["gh-skill"]);
    let checkout_change = summary.changed_skills[0].checkout_change.as_ref().unwrap();
    assert_eq!(checkout_change.previous.as_deref(), Some(old_head.as_str()));
    assert_eq!(checkout_change.current, new_head);
    assert!(summary.lock_written);
    assert_eq!(summary.repaired, 1, "the target link is repointed");
    assert_eq!(summary.links_unchanged, 1, "the local skill is untouched");

    assert_eq!(locked_commit(&fixture.world, "gh-skill"), new_head);
    let new_dest = link_dest(&fixture.world, "links/gh-skill");
    assert_ne!(new_dest, old_dest);
    assert!(new_dest.join("extra.md").exists());
    assert!(
      !old_dest.exists(),
      "the unreferenced old snapshot is pruned"
    );
  }

  #[test]
  fn moved_tags_resolve_to_their_new_commit() {
    let fixture = fixture();
    git(&fixture.remote.work, &["tag", "v1"]);
    git(
      &fixture.remote.work,
      &[
        "push",
        fixture.remote.bare.to_str().unwrap(),
        "refs/tags/v1",
      ],
    );
    fixture
      .world
      .write_config(&CONFIG.replace("ref = \"main\"", "ref = \"v1\""));

    let env = fixture.world.git_env();
    execute(&env).unwrap();
    let before = locked_commit(&fixture.world, "gh-skill");

    // move the tag to a new commit and force-push it
    let new_head = fixture
      .remote
      .push_new_commit("skills/gh-skill/extra.md", "tagged later\n");
    git(&fixture.remote.work, &["tag", "-f", "v1"]);
    git(
      &fixture.remote.work,
      &[
        "push",
        fixture.remote.bare.to_str().unwrap(),
        "+refs/tags/v1:refs/tags/v1",
      ],
    );

    let summary = execute(&env).unwrap();
    assert_eq!(changed_names(&summary), ["gh-skill"]);
    let checkout_change = summary.changed_skills[0].checkout_change.as_ref().unwrap();
    assert_eq!(checkout_change.previous.as_deref(), Some(before.as_str()));
    assert_eq!(checkout_change.current, new_head);
    let after = locked_commit(&fixture.world, "gh-skill");
    assert_ne!(before, after);
    assert_eq!(after, new_head);
  }

  #[test]
  fn fixed_commits_stay_fixed_and_need_no_network_once_cached() {
    let fixture = fixture();
    let pinned = fixture.remote.head_sha();
    fixture
      .world
      .write_config(&CONFIG.replace("ref = \"main\"", &format!("ref = \"{pinned}\"")));

    execute(&fixture.world.git_env()).unwrap();
    fixture
      .remote
      .push_new_commit("skills/gh-skill/extra.md", "moved on\n");

    // offline env: a fixed commit with a populated store never touches git
    let summary = execute(&fixture.world.offline_env()).unwrap();
    assert!(summary.changed_skills.is_empty());
    assert!(!summary.lock_written);
    assert_eq!(locked_commit(&fixture.world, "gh-skill"), pinned);
  }

  #[test]
  fn local_changes_are_picked_up() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();
    let old_dest = link_dest(&fixture.world, "links/local-skill");

    fs::write(
      fixture.world.home.join("skills/local-skill/new.md"),
      "local edit\n",
    )
    .unwrap();

    let summary = execute(&env).unwrap();
    assert_eq!(changed_names(&summary), ["local-skill"]);
    assert_eq!(summary.changed_skills[0].checkout_change, None);
    assert_eq!(summary.repaired, 1);

    let new_dest = link_dest(&fixture.world, "links/local-skill");
    assert_ne!(new_dest, old_dest);
    assert!(new_dest.join("new.md").exists());
    assert!(!old_dest.exists(), "the old local snapshot is pruned");
  }

  #[test]
  fn malformed_lock_state_is_regenerated_but_newer_is_refused() {
    let fixture = fixture();
    let env = fixture.world.git_env();

    fs::create_dir_all(fixture.world.paths().lockfile.parent().unwrap()).unwrap();
    fs::write(fixture.world.paths().lockfile, "not [ toml").unwrap();
    let summary = execute(&env).unwrap();
    assert!(summary.lock_written);
    assert_eq!(changed_names(&summary), ["gh-skill", "local-skill"]);
    assert_eq!(
      summary.changed_skills[0]
        .checkout_change
        .as_ref()
        .unwrap()
        .previous,
      None
    );

    fs::write(fixture.world.paths().lockfile, "version = 9\n").unwrap();
    let error = execute(&env).unwrap_err();
    assert!(error.to_string().contains("will not be overwritten"));
  }

  #[test]
  fn preparation_failures_change_nothing() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();
    let lock_before = fixture.world.lock_bytes();
    let dest_before = link_dest(&fixture.world, "links/gh-skill");

    // an unresolvable ref fails preparation before any visible change
    fixture
      .world
      .write_config(&CONFIG.replace("ref = \"main\"", "ref = \"no-such-ref\""));
    let error = execute(&env).unwrap_err();
    assert!(error.to_string().contains("was not found"));

    assert_eq!(fixture.world.lock_bytes(), lock_before);
    assert_eq!(link_dest(&fixture.world, "links/gh-skill"), dest_before);
  }

  #[test]
  fn a_blocked_target_aborts_lock_and_links_together() {
    let fixture = fixture();
    let env = fixture.world.git_env();

    // a regular file squats on one target: the whole commit must not happen
    let blocked = fixture.world.home.join("links/gh-skill");
    fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    fs::write(&blocked, "user data").unwrap();

    let error = execute(&env).unwrap_err();
    assert!(error.to_string().contains("refusing to replace"));

    assert!(
      !fixture.world.paths().lockfile.exists(),
      "no lockfile may be written when the link commit fails"
    );
    assert!(!fixture.world.home.join("links/local-skill").exists());
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "user data");
  }

  #[test]
  fn commit_failures_roll_back_lock_and_earlier_links() {
    // two local skills, both changing, so the transaction holds: lock write,
    // then two link repoints in name order
    let world = testutil::world();
    for name in ["skill-one", "skill-two"] {
      write_skill_md(&world.home.join("skills").join(name), name);
    }
    world.write_config(
      r#"version = 1

[skills.skill-one]
source = "skills/skill-one"
targets = ["links/skill-one"]

[skills.skill-two]
source = "skills/skill-two"
targets = ["links/skill-two"]
"#,
    );
    let env = world.git_env();
    execute(&env).unwrap();

    let lock_before = world.lock_bytes();
    let one_before = link_dest(&world, "links/skill-one");
    for name in ["skill-one", "skill-two"] {
      fs::write(world.home.join("skills").join(name).join("new.md"), "x").unwrap();
    }

    // sabotage skill-two's target AFTER staging: the commit writes the lock
    // and repoints skill-one before failing on skill-two's changed state
    let two = world.home.join("links/skill-two");
    let error = testutil::retry_lock(|| {
      execute_with_hook(&env, &mut || {
        if fs::symlink_metadata(&two)
          .map(|meta| meta.file_type().is_symlink())
          .unwrap_or(false)
        {
          fs::remove_file(&two).unwrap();
          fs::write(&two, "user data").unwrap();
        }
        Ok(())
      })
    })
    .unwrap_err();
    assert!(
      error.to_string().contains("changed since it was planned"),
      "unexpected error: {error:#}"
    );

    // everything the commit had already applied was restored
    assert_eq!(
      world.lock_bytes(),
      lock_before,
      "the lock write was rolled back"
    );
    assert_eq!(
      link_dest(&world, "links/skill-one"),
      one_before,
      "skill-one's already-applied repoint was rolled back"
    );
    assert_eq!(fs::read_to_string(&two).unwrap(), "user data");
  }

  #[test]
  fn an_external_lock_edit_aborts_even_when_the_lock_is_unchanged() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();

    // second run computes an identical lockfile (no transaction write would
    // cover it); an external edit mid-run must still abort
    let lock_path = fixture.world.paths().lockfile;
    let error = testutil::retry_lock(|| {
      execute_with_hook(&env, &mut || {
        let mut bytes = fs::read(&lock_path).unwrap();
        bytes.extend_from_slice(b"\n# edited externally\n");
        fs::write(&lock_path, bytes).unwrap();
        Ok(())
      })
    })
    .unwrap_err();

    assert!(
      error.to_string().contains("skillpm.lock changed"),
      "unexpected error: {error:#}"
    );
  }

  fn snapshot_count(world: &World) -> usize {
    fs::read_dir(world.paths().store.join("sha256"))
      .map(|dir| dir.count())
      .unwrap_or(0)
  }

  #[test]
  fn failed_updates_leave_no_new_snapshots_behind() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();
    assert_eq!(snapshot_count(&fixture.world), 2);

    // the local source changes (a new snapshot will be created during
    // preparation), then the github ref fails to resolve
    fs::write(
      fixture.world.home.join("skills/local-skill/new.md"),
      "local edit\n",
    )
    .unwrap();
    fixture
      .world
      .write_config(&CONFIG.replace("ref = \"main\"", "ref = \"no-such-ref\""));

    execute(&env).unwrap_err();
    assert_eq!(
      snapshot_count(&fixture.world),
      2,
      "the new local snapshot from the failed run must be cleaned up"
    );

    // and a post-preparation failure (blocked target) cleans up too
    fixture.world.write_config(CONFIG);
    let blocked = fixture.world.home.join("links/gh-skill");
    fs::remove_file(&blocked).unwrap();
    fs::write(&blocked, "user data").unwrap();

    execute(&env).unwrap_err();
    assert_eq!(snapshot_count(&fixture.world), 2);
  }

  #[test]
  #[cfg(unix)]
  fn prune_problems_are_warnings_not_failures() {
    let fixture = fixture();
    let env = fixture.world.git_env();
    execute(&env).unwrap();

    // sabotage pruning: the staging dir becomes a symlink prune refuses
    let staging = fixture.world.paths().store.join("staging");
    let _ = fs::remove_dir_all(&staging);
    std::os::unix::fs::symlink(fixture.world.temp.path(), &staging).unwrap();

    let summary = execute(&env).unwrap();
    assert!(summary.prune_warnings > 0, "expected a prune warning");
  }
}
