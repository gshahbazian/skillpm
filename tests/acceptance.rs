//! Black-box acceptance suite: drives the released `skillpm` binary across real
//! process and filesystem boundaries with isolated HOME roots and local
//! GitHub-style remotes (reached through an isolated git global config that
//! rewrites https://github.com/ via url.insteadOf — the production URL path).
//!
//! README section 12 completion criteria, mapped to automated coverage:
//! - deterministic parsing/lock/snapshot hashes ....... `lock_and_snapshots_are_deterministic`
//!   (plus golden hash vectors in src/snapshot.rs and lock goldens in src/lockfile.rs)
//! - reproducible offline installs .................... `install_is_offline_with_a_populated_store`
//! - exact locked reconstruction after cache deletion . `cache_deletion_reconstructs_exact_locked_versions`
//! - local/GitHub updates with unchanged fast paths ... `full_lifecycle_add_install_update_remove`
//! - strict source/skill/config/lock/target checks .... `strict_validation_rejects_bad_input`
//! - no replacement/deletion of regular targets ....... `regular_targets_are_never_replaced_or_deleted`
//! - rollback of ordinary failures .................... `failed_updates_roll_back_all_metadata`
//!   (staging-time abort; true mid-commit rollback across skills and targets
//!   is `commit_failures_roll_back_lock_and_earlier_links` in
//!   src/commands/update.rs, plus per-step fault injection in src/transaction.rs)
//! - comment preservation and symlinked config ........ `comments_and_symlinked_configs_survive`
//! - concurrent-command exclusion ..................... `concurrent_commands_are_excluded`
//! - auth redaction, timeout cleanup, git fallback .... `git_timeouts_fail_fast_and_clean`
//!   (redaction and token/fallback order require a failing-auth remote and are
//!   covered by fake-git tests in src/github.rs)
//! - pruning and read-only snapshot integrity ......... `snapshots_are_read_only_and_self_healing`
//! - idempotent install/update/repeated add ........... `full_lifecycle_add_install_update_remove`
//!
//! Unsupported-platform handling (Windows error) cannot execute on this
//! platform; it is compile-time guarded and unit-tested in src/platform.rs.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Sandbox {
  root: tempfile::TempDir,
}

fn sandbox() -> Sandbox {
  let root = tempfile::tempdir().unwrap();
  fs::create_dir(root.path().join("home")).unwrap();
  let sandbox = Sandbox { root };
  sandbox.redirect_github_to(sandbox.root.path());
  sandbox
}

impl Sandbox {
  fn home(&self) -> PathBuf {
    self.root.path().join("home")
  }

  fn config_path(&self) -> PathBuf {
    self.home().join(".config/skillpm/skillpm.toml")
  }

  fn lock_path(&self) -> PathBuf {
    self.home().join(".config/skillpm/skillpm.lock")
  }

  fn store_path(&self) -> PathBuf {
    self.home().join(".local/share/skillpm/store")
  }

  /// Writes an isolated git global config that rewrites https://github.com/
  /// to a local file:// base — skillpm's git children exercise the production
  /// URL path, and the host's real global config is never consulted.
  fn redirect_github_to(&self, base_root: &Path) {
    fs::write(
      self.git_config(),
      format!(
        "[url \"file://{}/\"]\n\tinsteadOf = https://github.com/\n",
        base_root.display()
      ),
    )
    .unwrap();
  }

  fn git_config(&self) -> PathBuf {
    self.root.path().join("gitconfig")
  }

  fn skillpm_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skillpm"));
    command
      .args(args)
      .env("HOME", self.home())
      .env("NO_COLOR", "1")
      .env("GIT_CONFIG_GLOBAL", self.git_config())
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .env_remove("GIT_CONFIG_COUNT")
      .env_remove("GIT_CONFIG_PARAMETERS")
      .env_remove("GIT_DIR")
      .env_remove("XDG_CONFIG_HOME")
      .env_remove("XDG_DATA_HOME")
      .env_remove("GITHUB_TOKEN")
      .env_remove("GH_TOKEN");
    for (key, value) in env {
      command.env(key, value);
    }
    command.output().expect("failed to run skillpm")
  }

  fn skillpm(&self, args: &[&str]) -> Output {
    self.skillpm_with(args, &[])
  }

  /// Success is required; returns (stdout, stderr). The stdout contract is
  /// one concise summary line.
  fn ok(&self, args: &[&str]) -> (String, String) {
    let output = self.skillpm(args);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
      output.status.success(),
      "skillpm {args:?} failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
      stdout.lines().count(),
      1,
      "stdout must be one summary line: {stdout:?}"
    );
    assert!(
      !stdout.contains('\x1b') && !stderr.contains('\x1b'),
      "NO_COLOR must strip ANSI"
    );
    (stdout, stderr)
  }

  /// Failure is required; returns stderr and asserts stdout stayed clean.
  fn err(&self, args: &[&str]) -> String {
    let output = self.skillpm(args);
    assert!(
      !output.status.success(),
      "skillpm {args:?} unexpectedly succeeded"
    );
    assert!(
      output.stdout.is_empty(),
      "stdout must stay clean on failure"
    );
    String::from_utf8(output.stderr).unwrap()
  }

  fn write_skill(&self, rel_dir: &str, name: &str) {
    let dir = self.home().join(rel_dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
      dir.join("SKILL.md"),
      format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
    )
    .unwrap();
  }

  fn read(&self, path: &Path) -> String {
    String::from_utf8(fs::read(path).unwrap()).unwrap()
  }
}

fn git(dir: &Path, args: &[&str]) {
  let output = Command::new("git")
    .args(args)
    .current_dir(dir)
    // hermetic: no host system/global config (e.g. commit.gpgsign) and no
    // inherited GIT_CONFIG_* injection may leak into fixtures
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env_remove("GIT_CONFIG_COUNT")
    .env_remove("GIT_CONFIG_PARAMETERS")
    .env_remove("GIT_DIR")
    .env("GIT_AUTHOR_NAME", "t")
    .env("GIT_AUTHOR_EMAIL", "t@t")
    .env("GIT_COMMITTER_NAME", "t")
    .env("GIT_COMMITTER_EMAIL", "t@t")
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "git {args:?} failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
}

/// A bare remote at <base>/<owner>/<repo>.git with skills under skills/.
fn make_remote(base: &Path, owner: &str, repo: &str, skills: &[&str]) -> PathBuf {
  let work = base.join(format!("work-{owner}-{repo}"));
  fs::create_dir_all(&work).unwrap();
  git(&work, &["init", "-b", "main", "."]);
  for name in skills {
    let dir = work.join("skills").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
      dir.join("SKILL.md"),
      format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
    )
    .unwrap();
  }
  git(&work, &["add", "."]);
  git(&work, &["commit", "-m", "skills"]);

  let bare = base.join(owner).join(format!("{repo}.git"));
  fs::create_dir_all(bare.parent().unwrap()).unwrap();
  git(
    base,
    &[
      "clone",
      "--bare",
      work.to_str().unwrap(),
      bare.to_str().unwrap(),
    ],
  );
  git(&bare, &["config", "uploadpack.allowanysha1inwant", "true"]);
  git(&bare, &["config", "uploadpack.allowfilter", "true"]);
  work
}

fn push_commit(work: &Path, bare: &Path, rel_file: &str, contents: &str) {
  fs::write(work.join(rel_file), contents).unwrap();
  git(work, &["add", "."]);
  git(work, &["commit", "-m", "update"]);
  git(work, &["push", bare.to_str().unwrap(), "main:main"]);
}

/// chmod -R u+w before deletion: committed snapshots are read-only.
fn force_remove(path: &Path) {
  let status = Command::new("chmod")
    .args(["-R", "u+w"])
    .arg(path)
    .status()
    .unwrap();
  assert!(status.success());
  fs::remove_dir_all(path).unwrap();
}

#[test]
fn full_lifecycle_add_install_update_remove() {
  let sandbox = sandbox();
  let work = make_remote(sandbox.root.path(), "owner", "repo", &["gh-skill"]);
  let bare = sandbox.root.path().join("owner/repo.git");
  sandbox.write_skill("skills/local-skill", "local-skill");

  // add: bootstraps everything
  let (stdout, _) = sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);
  assert!(stdout.starts_with("added skill 'local-skill'"), "{stdout}");
  let (stdout, _) = sandbox.ok(&[
    "add",
    "github:owner/repo/skills/gh-skill",
    "--target",
    "links/gh-skill",
    "--ref",
    "main",
  ]);
  assert!(stdout.starts_with("added skill 'gh-skill'"), "{stdout}");

  // repeated identical add: idempotent, config/lock byte-identical
  let config_before = sandbox.read(&sandbox.config_path());
  let lock_before = sandbox.read(&sandbox.lock_path());
  let (stdout, _) = sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);
  assert!(stdout.contains("already configured"), "{stdout}");
  assert_eq!(sandbox.read(&sandbox.config_path()), config_before);
  assert_eq!(sandbox.read(&sandbox.lock_path()), lock_before);

  // install: idempotent no-op on a fully linked machine
  let (stdout, _) = sandbox.ok(&["install"]);
  assert!(stdout.contains("2 unchanged"), "{stdout}");

  // update: unchanged fast path for both source kinds
  let (stdout, _) = sandbox.ok(&["update"]);
  assert!(stdout.contains("0 changed"), "{stdout}");
  assert_eq!(sandbox.read(&sandbox.lock_path()), lock_before);

  // update: both kinds change and targets repoint
  push_commit(&work, &bare, "skills/gh-skill/new.md", "remote change\n");
  fs::write(
    sandbox.home().join("skills/local-skill/new.md"),
    "local change\n",
  )
  .unwrap();
  let (stdout, _) = sandbox.ok(&["update"]);
  assert!(stdout.contains("2 changed"), "{stdout}");
  let gh_dest = fs::read_link(sandbox.home().join("links/gh-skill")).unwrap();
  assert!(
    gh_dest.join("new.md").exists(),
    "target repointed to new content"
  );

  // remove: one skill goes, the other stays; final removal leaves empty state
  let (stdout, _) = sandbox.ok(&["remove", "gh-skill"]);
  assert!(stdout.starts_with("removed skill 'gh-skill'"), "{stdout}");
  assert!(!sandbox.home().join("links/gh-skill").exists());
  assert!(sandbox.home().join("links/local-skill").exists());

  sandbox.ok(&["remove", "local-skill"]);
  assert_eq!(sandbox.read(&sandbox.lock_path()), "version = 1\n");
  assert!(!sandbox.read(&sandbox.config_path()).contains("[skills."));
}

#[test]
fn install_is_offline_with_a_populated_store() {
  let sandbox = sandbox();
  make_remote(sandbox.root.path(), "owner", "repo", &["gh-skill"]);
  sandbox.ok(&[
    "add",
    "github:owner/repo/skills/gh-skill",
    "--target",
    "links/gh-skill",
    "--ref",
    "main",
  ]);
  fs::remove_file(sandbox.home().join("links/gh-skill")).unwrap();

  // PATH without git: any git spawn would fail loudly
  let empty = sandbox.root.path().join("empty-path");
  fs::create_dir(&empty).unwrap();
  let output = sandbox.skillpm_with(&["install"], &[("PATH", empty.to_str().unwrap())]);
  assert!(
    output.status.success(),
    "offline install failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(sandbox.home().join("links/gh-skill").exists());
}

#[test]
fn cache_deletion_reconstructs_exact_locked_versions() {
  let sandbox = sandbox();
  let work = make_remote(sandbox.root.path(), "owner", "repo", &["gh-skill"]);
  let bare = sandbox.root.path().join("owner/repo.git");
  sandbox.ok(&[
    "add",
    "github:owner/repo/skills/gh-skill",
    "--target",
    "links/gh-skill",
    "--ref",
    "main",
  ]);

  // wipe the disposable cache entirely AND advance the remote
  force_remove(&sandbox.home().join(".local/share/skillpm"));
  push_commit(&work, &bare, "skills/gh-skill/new.md", "added later\n");

  sandbox.ok(&["install"]);
  let dest = fs::read_link(sandbox.home().join("links/gh-skill")).unwrap();
  assert!(dest.join("SKILL.md").exists());
  assert!(
    !dest.join("new.md").exists(),
    "install must reconstruct the locked commit, not the advanced branch"
  );
}

#[test]
fn lock_and_snapshots_are_deterministic() {
  // one shared remote, two independent machines
  let shared = sandbox();
  make_remote(shared.root.path(), "owner", "repo", &["gh-skill"]);

  let mut locks = Vec::new();
  let mut stores = Vec::new();
  for _ in 0..2 {
    let machine = sandbox();
    machine.redirect_github_to(shared.root.path());
    machine.write_skill("skills/local-skill", "local-skill");
    for args in [
      vec!["add", "skills/local-skill", "--target", "links/local-skill"],
      vec![
        "add",
        "github:owner/repo/skills/gh-skill",
        "--target",
        "links/gh-skill",
        "--ref",
        "main",
      ],
    ] {
      machine.ok(&args);
    }

    locks.push(machine.read(&machine.lock_path()));
    let mut hashes: Vec<String> = fs::read_dir(machine.store_path().join("sha256"))
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .collect();
    hashes.sort();
    stores.push(hashes);
  }

  assert_eq!(
    locks[0], locks[1],
    "lockfiles must be byte-identical across machines"
  );
  assert_eq!(
    stores[0], stores[1],
    "snapshot hashes must be identical across machines"
  );
}

#[test]
fn strict_validation_rejects_bad_input() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/local-skill", "local-skill");

  // malformed source spelling
  let stderr = sandbox.err(&["add", "https://github.com/a/b", "--target", "links/b"]);
  assert!(stderr.contains("not a supported source"), "{stderr}");

  // source without a SKILL.md
  fs::create_dir_all(sandbox.home().join("skills/empty")).unwrap();
  let stderr = sandbox.err(&["add", "skills/empty", "--target", "links/empty"]);
  assert!(stderr.contains("no SKILL.md"), "{stderr}");

  // target basename must equal the skill name
  let stderr = sandbox.err(&["add", "skills/local-skill", "--target", "links/renamed"]);
  assert!(stderr.contains("must end in the skill name"), "{stderr}");

  // config strictness: an unknown field poisons every command
  sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);
  let config = sandbox.read(&sandbox.config_path());
  fs::write(
    sandbox.config_path(),
    format!("unknown_field = 1\n{config}"),
  )
  .unwrap();
  let stderr = sandbox.err(&["install"]);
  assert!(stderr.contains("unknown top-level field"), "{stderr}");
  fs::write(sandbox.config_path(), config).unwrap();

  // stale lock: install/add/remove demand freshness
  fs::write(sandbox.lock_path(), "version = 1\n").unwrap();
  let stderr = sandbox.err(&["install"]);
  assert!(stderr.contains("run `skillpm update`"), "{stderr}");
}

#[test]
fn regular_targets_are_never_replaced_or_deleted() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/local-skill", "local-skill");

  // a user file at the target survives an add...
  let blocked = sandbox.home().join("links/local-skill");
  fs::create_dir_all(blocked.parent().unwrap()).unwrap();
  fs::write(&blocked, "user data").unwrap();
  let stderr = sandbox.err(&["add", "skills/local-skill", "--target", "links/local-skill"]);
  assert!(stderr.contains("refusing to replace"), "{stderr}");
  assert_eq!(sandbox.read(&blocked), "user data");

  // ...and a user directory with content survives a remove attempt
  fs::remove_file(&blocked).unwrap();
  sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);
  fs::remove_file(&blocked).unwrap();
  fs::create_dir(&blocked).unwrap();
  fs::write(blocked.join("precious"), "data").unwrap();
  let stderr = sandbox.err(&["remove", "local-skill"]);
  assert!(stderr.contains("refusing to remove"), "{stderr}");
  assert_eq!(sandbox.read(&blocked.join("precious")), "data");
}

#[test]
fn failed_updates_roll_back_all_metadata() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/skill-one", "skill-one");
  sandbox.write_skill("skills/skill-two", "skill-two");
  sandbox.ok(&["add", "skills/skill-one", "--target", "links/skill-one"]);
  sandbox.ok(&["add", "skills/skill-two", "--target", "links/skill-two"]);
  let lock_before = sandbox.read(&sandbox.lock_path());

  // change both sources, then block one target: nothing may change
  fs::write(sandbox.home().join("skills/skill-one/new.md"), "x").unwrap();
  fs::write(sandbox.home().join("skills/skill-two/new.md"), "x").unwrap();
  fs::remove_file(sandbox.home().join("links/skill-two")).unwrap();
  fs::write(sandbox.home().join("links/skill-two"), "user data").unwrap();

  let stderr = sandbox.err(&["update"]);
  assert!(stderr.contains("refusing to replace"), "{stderr}");
  assert_eq!(
    sandbox.read(&sandbox.lock_path()),
    lock_before,
    "lock rolled back"
  );
  let one = fs::read_link(sandbox.home().join("links/skill-one")).unwrap();
  assert!(!one.join("new.md").exists(), "no target may advance");
}

#[test]
fn comments_and_symlinked_configs_survive() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/skill-one", "skill-one");
  sandbox.write_skill("skills/skill-two", "skill-two");

  // the global config is a symlink to a dotfiles-style location
  let dotfiles = sandbox.home().join("dotfiles/skillpm.toml");
  fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
  fs::write(&dotfiles, "# managed in dotfiles\nversion = 1\n").unwrap();
  fs::create_dir_all(sandbox.config_path().parent().unwrap()).unwrap();
  std::os::unix::fs::symlink(&dotfiles, sandbox.config_path()).unwrap();

  // a preexisting config demands fresh lock state; update regenerates it
  sandbox.ok(&["update"]);
  sandbox.ok(&["add", "skills/skill-one", "--target", "links/skill-one"]);
  sandbox.ok(&["add", "skills/skill-two", "--target", "links/skill-two"]);
  sandbox.ok(&["remove", "skill-one"]);

  let meta = fs::symlink_metadata(sandbox.config_path()).unwrap();
  assert!(
    meta.file_type().is_symlink(),
    "the config symlink must survive"
  );
  let real = sandbox.read(&dotfiles);
  assert!(
    real.starts_with("# managed in dotfiles\n"),
    "comments survive:\n{real}"
  );
  assert!(real.contains("[skills.skill-two]"));
  assert!(!real.contains("skill-one"));
}

#[test]
fn concurrent_commands_are_excluded() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/local-skill", "local-skill");
  sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);

  // this test process plays the role of a second skillpm holding the lock
  let lock_file =
    fs::File::open(sandbox.home().join(".local/share/skillpm/.operation.lock")).unwrap();
  lock_file.try_lock().unwrap();

  let stderr = sandbox.err(&["update"]);
  assert!(
    stderr.contains("another skillpm process is already running"),
    "{stderr}"
  );
}

#[test]
fn git_timeouts_fail_fast_and_clean() {
  let sandbox = sandbox();

  // a PATH shim git that hangs forever
  let shim_dir = sandbox.root.path().join("shim");
  fs::create_dir(&shim_dir).unwrap();
  // /bin/sleep by absolute path: the shim runs with the stripped PATH too
  fs::write(shim_dir.join("git"), "#!/bin/sh\n/bin/sleep 30\n").unwrap();
  use std::os::unix::fs::PermissionsExt;
  fs::set_permissions(shim_dir.join("git"), fs::Permissions::from_mode(0o755)).unwrap();

  let start = Instant::now();
  let output = sandbox.skillpm_with(
    &["add", "github:owner/repo/skills/x", "--target", "links/x"],
    &[
      ("PATH", shim_dir.to_str().unwrap()),
      ("SKILLPM_GIT_TIMEOUT_SECONDS", "1"),
    ],
  );
  assert!(!output.status.success());
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.contains("timed out"), "{stderr}");
  assert!(
    start.elapsed() < Duration::from_secs(10),
    "the hung git and its children must be killed: took {:?}",
    start.elapsed()
  );
}

#[test]
fn snapshots_are_read_only_and_self_healing() {
  use std::os::unix::fs::PermissionsExt;

  let sandbox = sandbox();
  sandbox.write_skill("skills/local-skill", "local-skill");
  sandbox.ok(&["add", "skills/local-skill", "--target", "links/local-skill"]);

  let snapshot = fs::read_link(sandbox.home().join("links/local-skill")).unwrap();
  let dir_mode = fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777;
  let file_mode = fs::metadata(snapshot.join("SKILL.md"))
    .unwrap()
    .permissions()
    .mode()
    & 0o777;
  assert_eq!(dir_mode, 0o555, "snapshot directories are read-only");
  assert_eq!(file_mode, 0o444, "snapshot files are read-only");

  // corrupt the snapshot; install must detect and repair it from the source
  fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o755)).unwrap();
  fs::set_permissions(snapshot.join("SKILL.md"), fs::Permissions::from_mode(0o644)).unwrap();
  fs::write(snapshot.join("SKILL.md"), "tampered").unwrap();

  let (stdout, stderr) = sandbox.ok(&["install"]);
  assert!(stdout.contains("1 snapshot(s) reconstructed"), "{stdout}");
  assert!(stderr.contains("reconstructing"), "progress goes to stderr");
  assert!(
    sandbox
      .read(&snapshot.join("SKILL.md"))
      .contains("name: local-skill")
  );

  // pruning: a content change strands the old snapshot, update reclaims it
  fs::write(sandbox.home().join("skills/local-skill/new.md"), "x").unwrap();
  sandbox.ok(&["update"]);
  assert!(
    !snapshot.exists(),
    "the unreferenced snapshot is pruned automatically"
  );
}

#[test]
fn global_lookup_ignores_the_working_directory() {
  let sandbox = sandbox();

  // a decoy config in the working directory must never be consulted
  let decoy_dir = sandbox.root.path().join("project");
  fs::create_dir(&decoy_dir).unwrap();
  fs::write(
    decoy_dir.join("skillpm.toml"),
    "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = [\"t/x\"]\n",
  )
  .unwrap();

  let output = Command::new(env!("CARGO_BIN_EXE_skillpm"))
    .args(["update"])
    .current_dir(&decoy_dir)
    .env("HOME", sandbox.home())
    .env("NO_COLOR", "1")
    .env_remove("XDG_CONFIG_HOME")
    .env_remove("XDG_DATA_HOME")
    .output()
    .unwrap();
  assert!(!output.status.success());
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(
    stderr.contains("run `skillpm add`"),
    "the cwd config must be ignored: {stderr}"
  );
}

#[test]
fn help_matches_the_documented_v1_surface() {
  let sandbox = sandbox();

  let output = sandbox.skillpm(&["--help"]);
  assert!(output.status.success());
  let help = String::from_utf8(output.stdout).unwrap();
  for command in ["install", "update", "add", "remove"] {
    assert!(
      help.contains(command),
      "missing '{command}' in help:\n{help}"
    );
  }
  // no undocumented surface: config override, force, selective ops, modes
  for forbidden in ["--config", "--force", "--json", "--quiet", "--all"] {
    assert!(
      !help.contains(forbidden),
      "undocumented '{forbidden}' in help:\n{help}"
    );
  }

  let output = sandbox.skillpm(&["add", "--help"]);
  let add_help = String::from_utf8(output.stdout).unwrap();
  assert!(add_help.contains("--target"));
  assert!(add_help.contains("--agent"));
  assert!(add_help.contains("--ref"));
  assert!(!add_help.contains("--force"));
  // the README documents lowercase agent values only
  for value in ["agents", "pi", "claude"] {
    assert!(
      add_help.contains(value),
      "missing agent value '{value}' in help:\n{add_help}"
    );
  }
  assert!(!add_help.contains("CLAUDE"), "{add_help}");
}

#[test]
fn agents_shorthand_installs_into_known_agent_directories() {
  let sandbox = sandbox();
  sandbox.write_skill("skills/local-skill", "local-skill");

  let (stdout, _) = sandbox.ok(&[
    "add",
    "skills/local-skill",
    "--agent",
    "claude",
    "--agent",
    "pi",
  ]);
  assert!(stdout.starts_with("added skill 'local-skill'"), "{stdout}");

  let config = sandbox.read(&sandbox.config_path());
  assert!(config.contains("~/.claude/skills/local-skill"), "{config}");
  assert!(
    config.contains("~/.pi/agent/skills/local-skill"),
    "{config}"
  );

  for relative in [".claude/skills/local-skill", ".pi/agent/skills/local-skill"] {
    let link = sandbox.home().join(relative);
    let destination = fs::read_link(&link).unwrap();
    assert!(
      destination.starts_with(sandbox.store_path()),
      "{destination:?}"
    );
    assert!(
      link.join("SKILL.md").exists(),
      "missing content at {relative}"
    );
  }

  // mixing the shorthand with an equivalent explicit target adds one location
  let (stdout, _) = sandbox.ok(&[
    "add",
    "skills/local-skill",
    "--target",
    ".claude/skills/local-skill",
    "--agent",
    "agents",
  ]);
  assert!(stdout.contains("1 target(s) added"), "{stdout}");
  let config = sandbox.read(&sandbox.config_path());
  assert_eq!(
    config.matches("claude/skills/local-skill").count(),
    1,
    "{config}"
  );
  assert!(config.contains("~/.agents/skills/local-skill"), "{config}");

  // install and remove treat shorthand targets like any other target
  sandbox.ok(&["install"]);
  sandbox.ok(&["remove", "local-skill"]);
  for relative in [
    ".claude/skills/local-skill",
    ".pi/agent/skills/local-skill",
    ".agents/skills/local-skill",
  ] {
    assert!(
      !sandbox.home().join(relative).exists(),
      "{relative} should be unlinked"
    );
  }
}
