//! Shared fixtures for command-workflow tests: a temporary home with the
//! default XDG layout, plus local bare git remotes reachable over file://.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::commands::CommandEnv;
use crate::github::GitClient;
use crate::paths::Paths;
use crate::store::Store;

pub(crate) struct World {
  pub temp: tempfile::TempDir,
  pub home: PathBuf,
}

pub(crate) fn world() -> World {
  let temp = tempfile::tempdir().unwrap();
  let home = temp.path().join("home");
  fs::create_dir(&home).unwrap();
  World { temp, home }
}

impl World {
  pub fn paths(&self) -> Paths {
    Paths {
      home: self.home.clone(),
      config_file: self.home.join(".config/spm/spm.toml"),
      lockfile: self.home.join(".config/spm/spm.lock"),
      data_root: self.home.join(".local/share/spm"),
      store: self.home.join(".local/share/spm/store"),
      operation_lock: self.home.join(".local/share/spm/.operation.lock"),
    }
  }

  pub fn create_runtime_dirs(&self) {
    self.paths().create_runtime_dirs().unwrap();
  }

  pub fn store(&self) -> Store {
    Store::new(&self.paths().store)
  }

  /// A git that does not exist: proves a flow never spawns git.
  pub fn offline_env(&self) -> CommandEnv {
    CommandEnv {
      paths: self.paths(),
      git: GitClient::new(
        PathBuf::from("/nonexistent/git"),
        Duration::from_secs(5),
        vec![],
        "https://github.com/".into(),
      ),
    }
  }

  /// Real git against file:// remotes under this world's temp dir.
  pub fn git_env(&self) -> CommandEnv {
    CommandEnv {
      paths: self.paths(),
      git: GitClient::new(
        PathBuf::from("git"),
        Duration::from_secs(60),
        vec![],
        format!("file://{}/", self.temp.path().display()),
      ),
    }
  }

  pub fn write_config(&self, contents: &str) {
    let path = self.paths().config_file;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
  }

  pub fn config_bytes(&self) -> Vec<u8> {
    fs::read(self.paths().config_file).unwrap()
  }

  pub fn lock_bytes(&self) -> Vec<u8> {
    fs::read(self.paths().lockfile).unwrap()
  }
}

/// Retries a command on transient operation-lock contention. Concurrently
/// forked test subprocesses briefly inherit lock descriptors between fork and
/// exec, so an acquire right after a release can spuriously see a held lock.
/// Production is immune: each spm process holds the lock until it exits.
pub(crate) fn retry_lock<T>(mut run: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
  for _ in 0..100 {
    match run() {
      Err(error) if error.to_string().contains("another spm process") => {
        std::thread::sleep(Duration::from_millis(10));
      }
      other => return other,
    }
  }
  run()
}

pub(crate) fn write_skill_md(dir: &Path, name: &str) {
  fs::create_dir_all(dir).unwrap();
  fs::write(
    dir.join("SKILL.md"),
    format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
  )
  .unwrap();
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
  let output = Command::new("git")
    .args(args)
    .current_dir(dir)
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
  String::from_utf8(output.stdout).unwrap().trim().to_string()
}

pub(crate) struct Remote {
  pub work: PathBuf,
  pub bare: PathBuf,
}

impl Remote {
  pub fn head_sha(&self) -> String {
    git(&self.work, &["rev-parse", "HEAD"])
  }

  pub fn push_new_commit(&self, file: &str, contents: &str) -> String {
    fs::write(self.work.join(file), contents).unwrap();
    git(&self.work, &["add", "."]);
    git(&self.work, &["commit", "-m", "update"]);
    git(
      &self.work,
      &["push", self.bare.to_str().unwrap(), "main:main"],
    );
    self.head_sha()
  }
}

/// A bare repo at <world.temp>/<owner>/<repo>.git containing nested skills at
/// skills/<name>, with SHA fetch and partial clone enabled.
pub(crate) fn make_remote(world: &World, owner: &str, repo: &str, skills: &[&str]) -> Remote {
  let work = world.temp.path().join(format!("work-{owner}-{repo}"));
  fs::create_dir(&work).unwrap();

  git(&work, &["init", "-b", "main", "."]);
  for name in skills {
    write_skill_md(&work.join("skills").join(name), name);
  }
  git(&work, &["add", "."]);
  git(&work, &["commit", "-m", "skills"]);

  let bare = world.temp.path().join(owner).join(format!("{repo}.git"));
  fs::create_dir_all(bare.parent().unwrap()).unwrap();
  git(
    world.temp.path(),
    &[
      "clone",
      "--bare",
      work.to_str().unwrap(),
      bare.to_str().unwrap(),
    ],
  );
  git(&bare, &["config", "uploadpack.allowanysha1inwant", "true"]);
  git(&bare, &["config", "uploadpack.allowfilter", "true"]);

  Remote { work, bare }
}
