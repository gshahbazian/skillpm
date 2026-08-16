mod add;
mod install;
mod remove;
mod update;

use anyhow::{Context, Result, bail};

use crate::cli::Command;
use crate::github::{self, GitClient};
use crate::local;
use crate::lockfile::LockedSkill;
use crate::paths::{OperationLock, Paths, create_private_dir};
use crate::source::{Source, parse_source};
use crate::store::Store;

/// The process-level dependencies every command workflow needs; tests build
/// this against temporary homes and local file:// remotes.
pub struct CommandEnv {
  pub paths: Paths,
  pub git: GitClient,
}

impl CommandEnv {
  fn from_process() -> Result<Self> {
    Ok(Self {
      paths: Paths::from_env()?,
      git: GitClient::from_env(),
    })
  }
}

pub fn run(command: Command) -> Result<()> {
  match command {
    Command::Install => install::run(),
    Command::Update => update::run(),
    Command::Add {
      source,
      targets,
      r#ref,
    } => add::run(source, targets, r#ref),
    Command::Remove { name } => remove::run(name),
  }
}

/// Every command takes the single nonblocking lock before reading state.
/// The data root is a disposable cache: when a config already exists it may
/// be recreated by any command; only a truly fresh machine (no config) is
/// sent to `skillpm add`.
pub(crate) fn acquire_lock(paths: &Paths) -> Result<OperationLock> {
  if !paths.data_root.is_dir() {
    if !paths.config_file.exists() {
      bail!(
        "skillpm has not been set up yet (missing {}); run `skillpm add` to install a first skill",
        paths.config_file.display()
      );
    }
    create_private_dir(&paths.data_root)
      .with_context(|| format!("failed to recreate {}", paths.data_root.display()))?;
  }
  OperationLock::acquire(&paths.operation_lock)
}

/// Rebuilds a missing/corrupt snapshot at its exact locked version: GitHub
/// from the locked commit, local only on an exact source-hash match.
pub(crate) fn reconstruct_locked(
  env: &CommandEnv,
  store: &Store,
  name: &str,
  entry: &LockedSkill,
) -> Result<()> {
  let result = match parse_source(&entry.source)? {
    Source::GitHub(source) => {
      let commit = entry
        .commit
        .as_deref()
        .context("GitHub lock entry has no commit")?;
      github::reconstruct_github_snapshot(&env.git, store, &source, commit, &entry.content_hash)
    }
    Source::Local(source) => {
      local::reconstruct_local_snapshot(store, &source, &env.paths.home, &entry.content_hash)
    }
  };
  result.with_context(|| format!("failed to reconstruct the snapshot for skill '{name}'"))
}
