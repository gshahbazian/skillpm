use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::skill;
use crate::snapshot::{self, GitFilter, SnapshotTree};
use crate::source::GitHubSource;
use crate::store::{SnapshotStatus, Store};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Runs the system git noninteractively with buffered, redacted output.
#[derive(Debug)]
pub struct GitClient {
  program: PathBuf,
  timeout: Duration,
  /// Auth fallback order after credential helpers: GITHUB_TOKEN, then GH_TOKEN.
  tokens: Vec<String>,
  /// "https://github.com/" outside tests; tests point it at file:// remotes.
  remote_base: String,
}

impl GitClient {
  pub fn from_env() -> Self {
    let tokens = ["GITHUB_TOKEN", "GH_TOKEN"]
      .iter()
      .filter_map(|name| std::env::var(name).ok())
      .filter(|value| !value.is_empty())
      .collect();

    Self {
      program: PathBuf::from("git"),
      timeout: timeout_from(std::env::var("SKILLPM_GIT_TIMEOUT_SECONDS").ok().as_deref()),
      tokens,
      // always the real URL: black-box tests redirect it with an isolated
      // git config (url.<file-base>.insteadOf), not a runtime override
      remote_base: "https://github.com/".to_string(),
    }
  }

  /// Fully injectable constructor; production uses from_env, tests use this.
  #[cfg(test)]
  pub fn new(
    program: PathBuf,
    timeout: Duration,
    tokens: Vec<String>,
    remote_base: String,
  ) -> Self {
    Self {
      program,
      timeout,
      tokens,
      remote_base,
    }
  }

  pub fn remote_url(&self, source: &GitHubSource) -> String {
    format!("{}{}/{}.git", self.remote_base, source.owner, source.repo)
  }

  /// Resolves a source's optional ref to an exact commit. A full commit SHA
  /// stays fixed without any network access.
  pub fn resolve_ref(&self, source: &GitHubSource, reference: Option<&str>) -> Result<String> {
    if let Some(reference) = reference
      && is_full_sha(reference)
    {
      return Ok(reference.to_ascii_lowercase());
    }

    let url = self.remote_url(source);

    let Some(reference) = reference else {
      // remote default branch HEAD
      let stdout = self.run_with_auth(&["ls-remote".into(), url.clone(), "HEAD".into()])?;
      let Some((sha, _)) = parse_ls_remote(&stdout)
        .into_iter()
        .find(|(_, r)| r == "HEAD")
      else {
        bail!("could not resolve the default branch of {url}");
      };
      return validated_sha(&sha, "HEAD");
    };

    let branch_ref = format!("refs/heads/{reference}");
    let tag_ref = format!("refs/tags/{reference}");
    let peeled_ref = format!("{tag_ref}^{{}}");

    let stdout = self.run_with_auth(&[
      "ls-remote".into(),
      url.clone(),
      branch_ref.clone(),
      tag_ref.clone(),
      peeled_ref.clone(),
    ])?;
    let entries = parse_ls_remote(&stdout);

    let find = |name: &str| {
      entries
        .iter()
        .find(|(_, r)| r == name)
        .map(|(sha, _)| sha.clone())
    };
    let branch = find(&branch_ref);
    let tag = find(&tag_ref);
    let peeled = find(&peeled_ref);

    match (branch, tag) {
      (Some(_), Some(_)) => bail!(
        "'{reference}' is both a branch and a tag in {url}; use an unambiguous ref or a commit SHA"
      ),
      (Some(sha), None) => validated_sha(&sha, reference),
      // an annotated tag resolves to its peeled underlying commit
      (None, Some(tag_sha)) => validated_sha(&peeled.unwrap_or(tag_sha), reference),
      (None, None) => bail!("ref '{reference}' was not found in {url}"),
    }
  }

  /// First attempt lets ordinary credential helpers work; on a GitHub auth
  /// failure, retries once per available token. Non-auth failures never retry.
  fn run_with_auth(&self, args: &[String]) -> Result<String> {
    let stdout = self.run_with_auth_raw(args, &[])?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
  }

  fn run_with_auth_raw(&self, args: &[String], extra_env: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut attempts: Vec<Option<&str>> = vec![None];
    attempts.extend(self.tokens.iter().map(|token| Some(token.as_str())));

    let mut last_error = String::new();
    for token in attempts {
      let output = self.run_once(args, token, extra_env)?;
      if output.success {
        return Ok(output.stdout);
      }

      last_error = self.redact(&String::from_utf8_lossy(&output.stderr));
      if !looks_like_auth_failure(&last_error) {
        break;
      }
    }

    bail!(
      "git {} failed: {}",
      args.first().map(String::as_str).unwrap_or_default(),
      last_error.trim()
    )
  }

  fn run_once(
    &self,
    args: &[String],
    token: Option<&str>,
    extra_env: &[(&str, &str)],
  ) -> Result<RawOutput> {
    let mut command = Command::new(&self.program);
    command
      .args(args)
      .env("GIT_TERMINAL_PROMPT", "0")
      // GIT_TERMINAL_PROMPT does not cover askpass/GUI prompts: an askpass
      // that answers with an empty string beats one that pops a dialog, and
      // GCM_INTERACTIVE=never tames Git Credential Manager. Noninteractive
      // credential helpers still work normally.
      .env("GIT_ASKPASS", "echo")
      .env("SSH_ASKPASS", "echo")
      .env("GCM_INTERACTIVE", "never")
      .env("LC_ALL", "C") // stable messages for auth-failure detection
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());

    for (key, value) in extra_env {
      command.env(key, value);
    }

    // own process group, so a timeout can kill git AND its helper children
    #[cfg(unix)]
    {
      use std::os::unix::process::CommandExt;
      command.process_group(0);
    }

    // the credential travels only through child-process environment; it never
    // appears in argv, the URL, or anything a process listing could show
    if let Some(token) = token {
      command.env("GIT_CONFIG_COUNT", "1");
      command.env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader");
      command.env("GIT_CONFIG_VALUE_0", auth_header(token));
    }

    let mut child = match command.spawn() {
      Ok(child) => child,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        bail!(
          "git executable '{}' was not found; skillpm requires the system git",
          self.program.display()
        );
      }
      Err(error) => {
        return Err(error).with_context(|| format!("failed to run {}", self.program.display()));
      }
    };

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = self.wait_with_timeout(&mut child)?;
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();

    Ok(RawOutput {
      success: status,
      stdout,
      stderr,
    })
  }

  /// Polls the child until exit or deadline; a timed-out child is killed and
  /// reaped so nothing lingers.
  fn wait_with_timeout(&self, child: &mut Child) -> Result<bool> {
    let start = Instant::now();
    loop {
      match child.try_wait() {
        Ok(Some(status)) => return Ok(status.success()),
        Ok(None) => {}
        Err(error) => {
          kill_process_group(child);
          return Err(error).context("failed to wait for git");
        }
      }

      if start.elapsed() >= self.timeout {
        kill_process_group(child);
        bail!(
          "git timed out after {} seconds (override with SKILLPM_GIT_TIMEOUT_SECONDS)",
          self.timeout.as_secs_f64()
        );
      }

      thread::sleep(Duration::from_millis(10));
    }
  }

  /// Strips every known secret (tokens and derived headers) from text that
  /// could reach logs or error messages.
  fn redact(&self, text: &str) -> String {
    let mut out = text.to_string();
    for token in &self.tokens {
      out = out.replace(&auth_header(token), "[REDACTED]");
      out = out.replace(token, "[REDACTED]");
    }
    out
  }
}

/// What commands pass in per configured GitHub skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSkillRequest {
  /// Config key (or a placeholder for `add`); used for grouping and reporting.
  pub key: String,
  pub source: GitHubSource,
  pub r#ref: Option<String>,
  /// Locked state, enabling the unchanged fast path.
  pub locked: Option<LockedGitHub>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedGitHub {
  pub commit: String,
  pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedGitHubSkill {
  pub key: String,
  pub name: String,
  pub description: String,
  pub commit: String,
  pub content_hash: String,
  pub reused: bool,
  /// Whether this preparation created the snapshot (vs deduplicating);
  /// failed commands may clean up only created snapshots.
  pub created: bool,
}

const MAX_CONCURRENT_GROUPS: usize = 4;

/// Resolves, fetches, extracts, and validates every requested GitHub skill,
/// then commits snapshots to the store only after the whole preparation
/// phase has succeeded. Writes nothing but store data.
pub fn prepare_github_skills(
  client: &GitClient,
  store: &Store,
  requests: &[GitHubSkillRequest],
) -> Result<Vec<PreparedGitHubSkill>> {
  // one resolution and at most one fetch per repository/ref group;
  // BTreeMap gives deterministic group order for reporting and errors
  let mut groups: BTreeMap<(String, String, Option<String>), Vec<&GitHubSkillRequest>> =
    BTreeMap::new();
  for request in requests {
    groups
      .entry((
        request.source.owner.clone(),
        request.source.repo.clone(),
        request.r#ref.clone(),
      ))
      .or_default()
      .push(request);
  }
  let groups: Vec<Vec<&GitHubSkillRequest>> = groups.into_values().collect();

  // phase 1: everything fallible, bounded to four concurrent group jobs;
  // a failed batch stops later batches from launching
  let mut staged_groups: Vec<StagedGroup> = Vec::new();
  for batch in groups.chunks(MAX_CONCURRENT_GROUPS) {
    let results: Vec<Result<StagedGroup>> = std::thread::scope(|scope| {
      let handles: Vec<_> = batch
        .iter()
        .map(|group| scope.spawn(|| prepare_group(client, store, group)))
        .collect();
      handles.into_iter().map(join_group).collect()
    });

    for result in results {
      staged_groups.push(result?);
    }
  }

  let mut prepared = commit_phase(store, staged_groups)?;
  prepared.sort_by(|a, b| a.key.cmp(&b.key));
  Ok(prepared)
}

fn join_group<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
  match handle.join() {
    Ok(result) => result,
    Err(payload) => {
      let message = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
      } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
      } else {
        "unknown panic payload".to_string()
      };
      bail!("GitHub group preparation panicked: {message}")
    }
  }
}

/// Phase 2: sequential store commits, run only after every group staged
/// successfully. A mid-phase failure removes the snapshots this phase
/// created — the caller never learns about a partially committed batch.
fn commit_phase(
  store: &Store,
  staged_groups: Vec<StagedGroup>,
) -> Result<Vec<PreparedGitHubSkill>> {
  let mut created: Vec<String> = Vec::new();
  match commit_staged(store, staged_groups, &mut created) {
    Ok(prepared) => Ok(prepared),
    Err(error) => {
      for hash in &created {
        let _ = store.remove_snapshot(hash);
      }
      Err(error)
    }
  }
}

fn commit_staged(
  store: &Store,
  staged_groups: Vec<StagedGroup>,
  created: &mut Vec<String>,
) -> Result<Vec<PreparedGitHubSkill>> {
  let mut prepared = Vec::new();

  for group in staged_groups {
    for skill in group.skills {
      match skill.outcome {
        StagedOutcome::Reused { content_hash } => {
          let (name, description) = metadata_from_snapshot(store, &content_hash)?;
          prepared.push(PreparedGitHubSkill {
            key: skill.key,
            name,
            description,
            commit: skill.commit,
            content_hash,
            reused: true,
            created: false,
          });
        }
        StagedOutcome::Fresh { tree } => {
          let committed = store.commit_tree(&tree)?;
          // recorded before the metadata read, so an invalid snapshot is
          // cleaned up too
          if committed.created {
            created.push(committed.content_hash.clone());
          }
          let (name, description) = metadata_from_snapshot(store, &committed.content_hash)?;
          prepared.push(PreparedGitHubSkill {
            key: skill.key,
            name,
            description,
            commit: skill.commit,
            content_hash: committed.content_hash,
            reused: false,
            created: committed.created,
          });
        }
      }
    }
  }

  Ok(prepared)
}

/// For `install`: fetch the exact locked commit and rebuild the snapshot,
/// which must reproduce the locked hash exactly.
pub fn reconstruct_github_snapshot(
  client: &GitClient,
  store: &Store,
  source: &GitHubSource,
  commit: &str,
  locked_hash: &str,
) -> Result<()> {
  let temp = tempfile::tempdir().context("failed to create a temporary directory")?;
  let git_dir = temp.path().join("repo.git");
  client.fetch_commit(&client.remote_url(source), &git_dir, commit)?;

  let tree = stage_skill(client, &git_dir, &temp.path().join("skill"), commit, source)?;
  let committed = store.commit_tree(&tree)?;
  if committed.content_hash != locked_hash {
    if committed.created {
      let _ = store.remove_snapshot(&committed.content_hash);
    }
    bail!(
      "github:{}/{} at {commit} no longer reproduces its locked snapshot; run `skillpm update`",
      source.owner,
      source.repo
    );
  }

  Ok(())
}

struct StagedGroup {
  skills: Vec<StagedSkill>,
  /// Keeps unpacked trees alive until phase 2 commits them.
  _temp: Option<tempfile::TempDir>,
}

struct StagedSkill {
  key: String,
  commit: String,
  outcome: StagedOutcome,
}

enum StagedOutcome {
  Reused { content_hash: String },
  Fresh { tree: SnapshotTree },
}

fn prepare_group(
  client: &GitClient,
  store: &Store,
  requests: &[&GitHubSkillRequest],
) -> Result<StagedGroup> {
  let source = &requests[0].source;
  let commit = client.resolve_ref(source, requests[0].r#ref.as_deref())?;

  let mut skills = Vec::new();
  let mut pending: Vec<&GitHubSkillRequest> = Vec::new();
  for request in requests {
    // unchanged fast path: same commit and a verified snapshot skip the fetch
    if let Some(locked) = &request.locked
      && locked.commit == commit
      && store.verify_snapshot(&locked.content_hash)? == SnapshotStatus::Valid
    {
      skills.push(StagedSkill {
        key: request.key.clone(),
        commit: commit.clone(),
        outcome: StagedOutcome::Reused {
          content_hash: locked.content_hash.clone(),
        },
      });
      continue;
    }
    pending.push(request);
  }

  if pending.is_empty() {
    return Ok(StagedGroup {
      skills,
      _temp: None,
    });
  }

  let temp = tempfile::tempdir().context("failed to create a temporary directory")?;
  let git_dir = temp.path().join("repo.git");
  client.fetch_commit(&client.remote_url(source), &git_dir, &commit)?;

  for (index, request) in pending.into_iter().enumerate() {
    let unpack_dir = temp.path().join(format!("skill-{index}"));
    let tree = stage_skill(client, &git_dir, &unpack_dir, &commit, &request.source)
      .with_context(|| format!("failed to prepare skill '{}'", request.key))?;
    skills.push(StagedSkill {
      key: request.key.clone(),
      commit: commit.clone(),
      outcome: StagedOutcome::Fresh { tree },
    });
  }

  Ok(StagedGroup {
    skills,
    _temp: Some(temp),
  })
}

/// Extracts one selected path from the fetched commit into `unpack_dir` and
/// validates it, without committing anything to the store yet.
fn stage_skill(
  client: &GitClient,
  git_dir: &Path,
  unpack_dir: &Path,
  commit: &str,
  source: &GitHubSource,
) -> Result<SnapshotTree> {
  let path = source.path.as_deref();

  reject_submodules(client, git_dir, commit, path)?;

  let tar = client.archive(git_dir, commit, path)?;
  let written = crate::archive::unpack_tar(&tar, unpack_dir, path)?;
  if written == 0 {
    bail!(
      "path '{}' does not exist in commit {commit}",
      path.unwrap_or(".")
    );
  }

  let tree = snapshot::scan_tree(unpack_dir, GitFilter::IncludeAll)?;
  reject_lfs_pointers(unpack_dir, &tree)?;

  // fail the preparation phase before any snapshot is committed; the final
  // metadata is read from the immutable snapshot in phase 2
  skill::load_skill_metadata(unpack_dir)?;

  Ok(tree)
}

fn reject_submodules(
  client: &GitClient,
  git_dir: &Path,
  commit: &str,
  path: Option<&str>,
) -> Result<()> {
  let mut args = vec![
    "--git-dir".to_string(),
    git_dir.display().to_string(),
    "ls-tree".to_string(),
    "-r".to_string(),
    commit.to_string(),
  ];
  if let Some(path) = path {
    args.push("--".to_string());
    args.push(literal_pathspec(path));
  }

  let listing = String::from_utf8_lossy(&client.run_attr_clean(&args)?).into_owned();
  for line in listing.lines() {
    // "<mode> <type> <oid>\t<path>"; gitlinks are mode 160000
    if line.starts_with("160000 ") {
      let name = line.split('\t').nth(1).unwrap_or("?");
      bail!("'{name}' is a Git submodule; skills with submodules are not supported in v1");
    }
  }
  Ok(())
}

const LFS_VERSION: &[u8] = b"version https://git-lfs.github.com/spec/v1";

fn reject_lfs_pointers(root: &Path, tree: &SnapshotTree) -> Result<()> {
  for entry in tree.entries() {
    if !matches!(entry.kind, crate::snapshot::EntryKind::File { .. }) {
      continue;
    }
    let path = root.join(&entry.path);
    if is_lfs_pointer(&path)? {
      bail!(
        "'{}' is a Git LFS pointer; skills using LFS are not supported in v1",
        entry.path
      );
    }
  }
  Ok(())
}

fn is_lfs_pointer(path: &Path) -> Result<bool> {
  let file =
    std::fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
  let mut prefix = Vec::with_capacity(LFS_VERSION.len() + 2);
  file
    .take((LFS_VERSION.len() + 2) as u64)
    .read_to_end(&mut prefix)?;

  if !prefix.starts_with(LFS_VERSION) {
    return Ok(false);
  }

  let suffix = &prefix[LFS_VERSION.len()..];
  Ok(suffix.starts_with(b"\n") || suffix.starts_with(b"\r\n"))
}

fn literal_pathspec(path: &str) -> String {
  format!(":(literal){path}")
}

fn metadata_from_snapshot(store: &Store, content_hash: &str) -> Result<(String, String)> {
  let snapshot_dir = store.snapshot_path(content_hash)?;
  let metadata = skill::load_skill_metadata(&snapshot_dir)?;
  Ok((metadata.name, metadata.description))
}

impl GitClient {
  /// Shallow, blob-filtered fetch of one exact commit, with a plain
  /// depth-1 fallback when the server does not support partial fetch.
  pub(crate) fn fetch_commit(&self, url: &str, git_dir: &Path, commit: &str) -> Result<()> {
    let git_dir_arg = git_dir.display().to_string();

    self.run_with_auth(&[
      "init".to_string(),
      "--bare".to_string(),
      "--quiet".to_string(),
      git_dir_arg.clone(),
    ])?;
    self.run_with_auth(&[
      "--git-dir".to_string(),
      git_dir_arg.clone(),
      "remote".to_string(),
      "add".to_string(),
      "origin".to_string(),
      url.to_string(),
    ])?;

    let fetch = |filter: bool| -> Result<String> {
      let mut args = vec![
        "--git-dir".to_string(),
        git_dir_arg.clone(),
        "fetch".to_string(),
        "--quiet".to_string(),
        "--depth".to_string(),
        "1".to_string(),
      ];
      if filter {
        args.push("--filter=blob:none".to_string());
      }
      args.push("origin".to_string());
      args.push(commit.to_string());
      self.run_with_auth(&args)
    };

    match fetch(true) {
      Ok(_) => Ok(()),
      // fall back to a plain depth-1 fetch ONLY when the server rejects
      // filtering; auth, timeout, missing-commit, and network failures must
      // not double the work
      Err(error) if filter_unsupported(&format!("{error:#}")) => fetch(false)
        .map(|_| ())
        .context("failed to fetch the requested commit"),
      Err(error) => Err(error),
    }
  }

  /// `git archive` of the exact commit; the tar bytes cannot be altered by
  /// checkout filters because no working tree is ever involved.
  pub(crate) fn archive(
    &self,
    git_dir: &Path,
    commit: &str,
    path: Option<&str>,
  ) -> Result<Vec<u8>> {
    let mut args = vec![
      "--git-dir".to_string(),
      git_dir.display().to_string(),
      "archive".to_string(),
      "--format=tar".to_string(),
      commit.to_string(),
    ];
    if let Some(path) = path {
      // "--" plus :(literal) so a hostile path can be neither an option
      // (e.g. -o<file>) nor a glob
      args.push("--".to_string());
      args.push(literal_pathspec(path));
    }
    self.run_attr_clean(&args)
  }

  /// Local object reads with the user's global/system attributes neutralized,
  /// so eol/filters config cannot alter snapshot bytes. Global *config* stays
  /// available: a blob-filtered repo may lazily fetch objects during archive,
  /// which needs credential helpers.
  pub(crate) fn run_attr_clean(&self, args: &[String]) -> Result<Vec<u8>> {
    let mut full: Vec<String> = vec![
      "-c".to_string(),
      "core.attributesfile=".to_string(),
      "-c".to_string(),
      "core.autocrlf=false".to_string(),
    ];
    full.extend_from_slice(args);
    self.run_with_auth_raw(&full, &[("GIT_ATTR_NOSYSTEM", "1")])
  }
}

/// Kills git and every descendant in its process group; a lone child.kill()
/// leaves helper children alive holding the output pipes, which would hang
/// the reader threads past the timeout.
fn kill_process_group(child: &mut std::process::Child) {
  #[cfg(unix)]
  {
    // the child was spawned with process_group(0), so its pid is the pgid;
    // the negative pid targets the whole group
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
      // SAFETY: `pid` identifies the live child process, and `kill` does not
      // retain or dereference pointers. A negative pid addresses its group.
      #[allow(unsafe_code)] // no safe std API for killpg
      unsafe {
        libc::kill(-pid, libc::SIGKILL);
      }
    }
  }

  let _ = child.kill();
  let _ = child.wait();
}

struct RawOutput {
  success: bool,
  stdout: Vec<u8>,
  stderr: Vec<u8>,
}

fn drain(stream: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
  thread::spawn(move || {
    let mut buffer = Vec::new();
    if let Some(mut stream) = stream {
      let _ = stream.read_to_end(&mut buffer);
    }
    buffer
  })
}

fn timeout_from(value: Option<&str>) -> Duration {
  value
    .and_then(|value| value.trim().parse::<u64>().ok())
    .filter(|seconds| *seconds > 0)
    .map(Duration::from_secs)
    .unwrap_or(DEFAULT_TIMEOUT)
}

fn auth_header(token: &str) -> String {
  format!(
    "Authorization: Basic {}",
    base64(format!("x-access-token:{token}").as_bytes())
  )
}

/// The hard-fail spellings of "this server cannot do partial fetch" across
/// git/protocol versions. (Some clients instead warn "filtering not
/// recognized by server, ignoring" and succeed unfiltered — no fallback
/// needed there.)
fn filter_unsupported(error: &str) -> bool {
  let lower = error.to_lowercase();
  lower.contains("does not support filtering")
    || lower.contains("filtering not supported")
    || lower.contains("filtering capability not supported")
    || lower.contains("does not support filter")
    || lower.contains("invalid filter-spec")
    || (lower.contains("unknown option") && lower.contains("filter"))
}

fn looks_like_auth_failure(stderr: &str) -> bool {
  [
    "Authentication failed",
    "could not read Username",
    "could not read Password",
    "Invalid username or password",
    "terminal prompts disabled",
    "Repository not found",
    "HTTP 401",
    "HTTP 403",
  ]
  .iter()
  .any(|needle| stderr.contains(needle))
}

fn parse_ls_remote(stdout: &str) -> Vec<(String, String)> {
  stdout
    .lines()
    .filter_map(|line| {
      let (sha, name) = line.split_once('\t')?;
      Some((sha.to_string(), name.to_string()))
    })
    .collect()
}

fn validated_sha(sha: &str, reference: &str) -> Result<String> {
  if !is_full_sha(sha) {
    bail!("git returned a malformed object ID '{sha}' for '{reference}'");
  }
  Ok(sha.to_ascii_lowercase())
}

fn is_full_sha(text: &str) -> bool {
  text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn base64(input: &[u8]) -> String {
  const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

  for chunk in input.chunks(3) {
    let b = [
      chunk[0],
      *chunk.get(1).unwrap_or(&0),
      *chunk.get(2).unwrap_or(&0),
    ];
    let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);

    out.push(TABLE[(n >> 18) as usize & 63] as char);
    out.push(TABLE[(n >> 12) as usize & 63] as char);
    out.push(if chunk.len() > 1 {
      TABLE[(n >> 6) as usize & 63] as char
    } else {
      '='
    });
    out.push(if chunk.len() > 2 {
      TABLE[n as usize & 63] as char
    } else {
      '='
    });
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::path::Path;
  use std::process::Command;

  fn git(dir: &Path, args: &[&str]) -> String {
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

  /// A bare repo at <tmp>/owner/repo.git with main, a side branch, a
  /// lightweight tag, an annotated tag, and one branch/tag name collision.
  struct Remote {
    temp: tempfile::TempDir,
    work: PathBuf,
  }

  fn remote() -> Remote {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();

    git(&work, &["init", "-b", "main", "."]);
    fs::write(work.join("file"), "one\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "one"]);
    git(&work, &["tag", "light"]);
    git(&work, &["tag", "-a", "annotated", "-m", "note"]);
    git(&work, &["branch", "dual"]);
    git(&work, &["tag", "dual"]);

    let bare = temp.path().join("owner/repo.git");
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
      temp.path(),
      &[
        "clone",
        "--bare",
        work.to_str().unwrap(),
        bare.to_str().unwrap(),
      ],
    );

    Remote { temp, work }
  }

  impl Remote {
    fn client(&self) -> GitClient {
      GitClient::new(
        PathBuf::from("git"),
        Duration::from_secs(30),
        vec![],
        format!("file://{}/", self.temp.path().display()),
      )
    }

    fn source(&self) -> GitHubSource {
      GitHubSource {
        owner: "owner".into(),
        repo: "repo".into(),
        path: None,
      }
    }

    fn head_sha(&self) -> String {
      git(&self.work, &["rev-parse", "HEAD"])
    }

    fn push_new_commit(&self) -> String {
      fs::write(self.work.join("file"), "two\n").unwrap();
      git(&self.work, &["add", "."]);
      git(&self.work, &["commit", "-m", "two"]);

      // the bare repo was cloned FROM work, so push by path, not "origin"
      let bare = self.temp.path().join("owner/repo.git");
      git(&self.work, &["push", bare.to_str().unwrap(), "main:main"]);
      self.head_sha()
    }
  }

  fn fake_git(dir: &Path, script_body: &str) -> PathBuf {
    let path = dir.join("fake-git");
    fs::write(&path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
  }

  /// The deadline is generous on purpose: macOS charges a large one-time cost
  /// (~100ms idle, over a second when the suite runs 14-wide) the first time it
  /// execs a freshly written file, so a tight timeout makes every fake-git test
  /// flaky on a Mac while passing on Linux CI. Tests that assert the timeout
  /// itself use fake_client_with_timeout.
  fn fake_client(program: PathBuf, tokens: Vec<String>) -> GitClient {
    fake_client_with_timeout(program, tokens, Duration::from_secs(20))
  }

  fn fake_client_with_timeout(
    program: PathBuf,
    tokens: Vec<String>,
    timeout: Duration,
  ) -> GitClient {
    GitClient::new(program, timeout, tokens, "https://github.com/".into())
  }

  #[test]
  fn group_thread_panics_become_errors() {
    let error = std::thread::scope(|scope| {
      join_group(scope.spawn(|| -> Result<()> {
        panic!("injected group panic");
      }))
    })
    .unwrap_err();

    assert!(error.to_string().contains("injected group panic"));
  }

  #[test]
  fn lfs_detection_requires_the_exact_version_line() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");

    fs::write(
      &path,
      "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 5\n",
    )
    .unwrap();
    assert!(is_lfs_pointer(&path).unwrap());

    fs::write(
      &path,
      "version https://git-lfs.github.com/spec/v1\r\noid sha256:abc\r\nsize 5\r\n",
    )
    .unwrap();
    assert!(is_lfs_pointer(&path).unwrap());

    fs::write(&path, "version https://git-lfs-not-a-pointer\n").unwrap();
    assert!(!is_lfs_pointer(&path).unwrap());
  }

  #[test]
  fn resolves_default_head_branches_and_tags() {
    let remote = remote();
    let client = remote.client();
    let source = remote.source();
    let head = remote.head_sha();

    // default HEAD, explicit branch, lightweight tag, and the peeled
    // annotated tag all point at the same commit
    assert_eq!(client.resolve_ref(&source, None).unwrap(), head);
    assert_eq!(client.resolve_ref(&source, Some("main")).unwrap(), head);
    assert_eq!(client.resolve_ref(&source, Some("light")).unwrap(), head);
    assert_eq!(
      client.resolve_ref(&source, Some("annotated")).unwrap(),
      head
    );
  }

  #[test]
  fn moved_branches_resolve_to_the_new_commit() {
    let remote = remote();
    let client = remote.client();
    let source = remote.source();

    let before = client.resolve_ref(&source, Some("main")).unwrap();
    let after_sha = remote.push_new_commit();
    let after = client.resolve_ref(&source, Some("main")).unwrap();

    assert_ne!(before, after);
    assert_eq!(after, after_sha);
  }

  #[test]
  fn branch_tag_collisions_are_ambiguous() {
    let remote = remote();
    let error = remote
      .client()
      .resolve_ref(&remote.source(), Some("dual"))
      .unwrap_err();
    assert!(error.to_string().contains("both a branch and a tag"));
  }

  #[test]
  fn unknown_refs_are_rejected() {
    let remote = remote();
    let error = remote
      .client()
      .resolve_ref(&remote.source(), Some("no-such-ref"))
      .unwrap_err();
    assert!(error.to_string().contains("was not found"));
  }

  #[test]
  fn full_shas_stay_fixed_without_invoking_git() {
    // a nonexistent git binary proves no process is spawned
    let client = fake_client(PathBuf::from("/nonexistent/git"), vec![]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let sha = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(client.resolve_ref(&source, Some(sha)).unwrap(), sha);

    let upper = client
      .resolve_ref(&source, Some(&sha.to_uppercase()))
      .unwrap();
    assert_eq!(upper, sha, "SHAs normalize to lowercase");
  }

  #[test]
  fn missing_git_is_a_clear_error() {
    let client = fake_client(PathBuf::from("/nonexistent/git"), vec![]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let error = client.resolve_ref(&source, Some("main")).unwrap_err();
    assert!(
      error
        .to_string()
        .contains("skillpm requires the system git")
    );
  }

  #[test]
  #[cfg(unix)]
  fn timeouts_kill_the_child() {
    let temp = tempfile::tempdir().unwrap();
    let program = fake_git(temp.path(), "sleep 30");
    let client = fake_client_with_timeout(program, vec![], Duration::from_millis(400));
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let start = Instant::now();
    let error = client.resolve_ref(&source, Some("main")).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(
      start.elapsed() < Duration::from_secs(5),
      "the child must be killed, not waited for"
    );
  }

  #[test]
  #[cfg(unix)]
  fn timeouts_kill_helper_children_holding_the_pipes() {
    let temp = tempfile::tempdir().unwrap();
    // a background "helper" inherits stdout/stderr; killing only the parent
    // would leave it holding the pipes and hang the reader threads
    let program = fake_git(temp.path(), "sleep 30 &\nsleep 30");
    let client = fake_client_with_timeout(program, vec![], Duration::from_millis(400));
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let start = Instant::now();
    let error = client.resolve_ref(&source, Some("main")).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(
      start.elapsed() < Duration::from_secs(5),
      "the whole process group must die, not just git: took {:?}",
      start.elapsed()
    );
  }

  #[test]
  #[cfg(unix)]
  fn prompting_is_disabled_across_every_channel() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("env.log");
    let script = format!(
      r#"echo "$GIT_TERMINAL_PROMPT/$GIT_ASKPASS/$SSH_ASKPASS/$GCM_INTERACTIVE" > "{}"
echo "fatal: nope" >&2
exit 128"#,
      log.display(),
    );
    let program = fake_git(temp.path(), &script);
    let client = fake_client(program, vec![]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    client.resolve_ref(&source, Some("main")).unwrap_err();
    assert_eq!(
      fs::read_to_string(&log).unwrap().trim(),
      "0/echo/echo/never"
    );
  }

  #[test]
  #[cfg(unix)]
  fn auth_fallback_tries_helpers_then_each_token_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("attempts.log");
    let expected = auth_header("tok-gh");
    let sha = "0123456789abcdef0123456789abcdef01234567";

    // succeeds only for the second token (GH_TOKEN slot)
    let script = format!(
      r#"echo "${{GIT_CONFIG_VALUE_0:-none}}" >> "{log}"
if [ "${{GIT_CONFIG_VALUE_0:-}}" = "{expected}" ]; then
  printf '{sha}\trefs/heads/main\n'
  exit 0
fi
echo "fatal: Authentication failed for repo" >&2
exit 128"#,
      log = log.display(),
    );
    let program = fake_git(temp.path(), &script);
    let client = fake_client(program, vec!["tok-github".into(), "tok-gh".into()]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    assert_eq!(client.resolve_ref(&source, Some("main")).unwrap(), sha);

    let attempts: Vec<String> = fs::read_to_string(&log)
      .unwrap()
      .lines()
      .map(String::from)
      .collect();
    assert_eq!(
      attempts,
      vec![
        "none".to_string(),
        auth_header("tok-github"),
        auth_header("tok-gh"),
      ]
    );
  }

  #[test]
  #[cfg(unix)]
  fn non_auth_failures_do_not_retry_with_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("attempts.log");

    let script = format!(
      r#"echo "attempt" >> "{log}"
echo "fatal: unable to access: Could not resolve host" >&2
exit 128"#,
      log = log.display(),
    );
    let program = fake_git(temp.path(), &script);
    let client = fake_client(program, vec!["tok".into()]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    client.resolve_ref(&source, Some("main")).unwrap_err();
    let attempts = fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(attempts, 1, "network errors must not trigger token retries");
  }

  #[test]
  #[cfg(unix)]
  fn errors_never_expose_tokens_or_derived_headers() {
    let temp = tempfile::tempdir().unwrap();

    // a hostile/echoing git that leaks its credential env into stderr
    let script = r#"echo "leak: ${GIT_CONFIG_VALUE_0:-none} token" >&2
echo "fatal: Authentication failed" >&2
exit 128"#;
    let program = fake_git(temp.path(), script);
    let client = fake_client(program, vec!["sekrit-token".into()]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let error = format!(
      "{:#}",
      client.resolve_ref(&source, Some("main")).unwrap_err()
    );
    assert!(!error.contains("sekrit-token"), "raw token leaked: {error}");
    assert!(
      !error.contains(&base64(b"x-access-token:sekrit-token")),
      "derived header leaked: {error}"
    );
    assert!(error.contains("[REDACTED]"));
  }

  #[test]
  #[cfg(unix)]
  fn malformed_object_ids_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let script = r#"printf 'zzzz\trefs/heads/main\n'"#;
    let program = fake_git(temp.path(), script);
    let client = fake_client(program, vec![]);
    let source = GitHubSource {
      owner: "o".into(),
      repo: "r".into(),
      path: None,
    };

    let error = client.resolve_ref(&source, Some("main")).unwrap_err();
    assert!(error.to_string().contains("malformed object ID"));
  }

  /// A bare remote at <tmp>/owner/repo.git containing a repo-root skill and
  /// two nested skills, with SHA fetch and partial clone enabled.
  struct SkillRemote {
    temp: tempfile::TempDir,
    work: PathBuf,
    bare: PathBuf,
  }

  fn write_skill_md(dir: &Path, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
      dir.join("SKILL.md"),
      format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
    )
    .unwrap();
  }

  fn skill_remote() -> SkillRemote {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();

    git(&work, &["init", "-b", "main", "."]);
    write_skill_md(&work, "root-skill");
    write_skill_md(&work.join("skills/skill-a"), "skill-a");
    fs::write(work.join("skills/skill-a/extra.md"), "extra\n").unwrap();
    write_skill_md(&work.join("skills/skill-b"), "skill-b");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "skills"]);

    let bare = temp.path().join("owner/repo.git");
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
      temp.path(),
      &[
        "clone",
        "--bare",
        work.to_str().unwrap(),
        bare.to_str().unwrap(),
      ],
    );
    git(&bare, &["config", "uploadpack.allowanysha1inwant", "true"]);
    git(&bare, &["config", "uploadpack.allowfilter", "true"]);

    SkillRemote { temp, work, bare }
  }

  impl SkillRemote {
    /// A client whose git logs every invocation, then delegates to real git.
    fn logging_client(&self, log: &Path) -> GitClient {
      let script = format!(
        r#"echo "$*" >> "{}"
exec git "$@""#,
        log.display()
      );
      let program = fake_git(self.temp.path(), &script);
      GitClient::new(
        program,
        Duration::from_secs(60),
        vec![],
        format!("file://{}/", self.temp.path().display()),
      )
    }

    fn store(&self) -> Store {
      Store::new(&self.temp.path().join("store"))
    }

    fn source(&self, path: Option<&str>) -> GitHubSource {
      GitHubSource {
        owner: "owner".into(),
        repo: "repo".into(),
        path: path.map(String::from),
      }
    }

    fn request(&self, key: &str, path: Option<&str>) -> GitHubSkillRequest {
      GitHubSkillRequest {
        key: key.into(),
        source: self.source(path),
        r#ref: Some("main".into()),
        locked: None,
      }
    }
  }

  fn fetch_count(log: &Path) -> usize {
    fs::read_to_string(log)
      .unwrap_or_default()
      .lines()
      .filter(|line| line.contains(" fetch "))
      .count()
  }

  #[test]
  fn prepares_root_and_nested_skills_with_one_fetch_per_group() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let requests = vec![
      remote.request("root-skill", None),
      remote.request("skill-a", Some("skills/skill-a")),
      remote.request("skill-b", Some("skills/skill-b")),
    ];
    let prepared = prepare_github_skills(&client, &store, &requests).unwrap();

    assert_eq!(prepared.len(), 3);
    for skill in &prepared {
      assert_eq!(skill.key, skill.name);
      assert_eq!(skill.description, format!("The {} skill.", skill.name));
      assert!(!skill.reused);
      assert_eq!(
        store.verify_snapshot(&skill.content_hash).unwrap(),
        SnapshotStatus::Valid
      );
    }

    // the root skill snapshot contains the nested ones (repo root), while a
    // nested one is prefix-stripped to its own content
    let a = prepared.iter().find(|s| s.name == "skill-a").unwrap();
    let a_dir = store.snapshot_path(&a.content_hash).unwrap();
    assert!(a_dir.join("SKILL.md").exists());
    assert!(a_dir.join("extra.md").exists());
    assert!(!a_dir.join("skills").exists());

    assert_eq!(
      fetch_count(&log),
      1,
      "three skills from one repo/ref must share one fetch"
    );
  }

  #[test]
  fn unchanged_locked_skills_skip_fetching_entirely() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let first = prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap();

    fs::remove_file(&log).unwrap();
    let mut request = remote.request("skill-a", Some("skills/skill-a"));
    request.locked = Some(LockedGitHub {
      commit: first[0].commit.clone(),
      content_hash: first[0].content_hash.clone(),
    });
    let second = prepare_github_skills(&client, &store, &[request]).unwrap();

    assert!(second[0].reused);
    assert_eq!(second[0].content_hash, first[0].content_hash);
    assert_eq!(fetch_count(&log), 0, "unchanged skills must not fetch");
  }

  #[test]
  fn preparing_succeeds_when_the_server_refuses_filtering() {
    // end-to-end sanity with a real remote that disallows filters; whether
    // this git warns-and-ignores or hard-fails, preparation must succeed
    // (the fallback-vs-no-fallback distinction is tested deterministically
    // below with fake gits)
    let remote = skill_remote();
    git(&remote.bare, &["config", "uploadpack.allowfilter", "false"]);

    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let prepared = prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap();

    assert_eq!(
      store.verify_snapshot(&prepared[0].content_hash).unwrap(),
      SnapshotStatus::Valid
    );
  }

  #[test]
  #[cfg(unix)]
  fn unsupported_filter_hard_failures_fall_back_exactly_once() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");

    // a git whose filtered fetches hard-fail the way older clients do
    let script = format!(
      r#"echo "$*" >> "{}"
case "$*" in
  *--filter*) echo "fatal: server does not support filtering" >&2; exit 128;;
esac
exec git "$@""#,
      log.display(),
    );
    let program = fake_git(remote.temp.path(), &script);
    let client = GitClient::new(
      program,
      Duration::from_secs(60),
      vec![],
      format!("file://{}/", remote.temp.path().display()),
    );
    let store = remote.store();

    let prepared = prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap();

    assert_eq!(
      store.verify_snapshot(&prepared[0].content_hash).unwrap(),
      SnapshotStatus::Valid
    );
    assert_eq!(fetch_count(&log), 2, "one filtered attempt, one fallback");
  }

  #[test]
  #[cfg(unix)]
  fn non_filter_fetch_failures_do_not_fall_back() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");

    let script = format!(
      r#"echo "$*" >> "{}"
case "$*" in
  *" fetch "*) echo "fatal: unable to access: Could not resolve host" >&2; exit 128;;
esac
exec git "$@""#,
      log.display(),
    );
    let program = fake_git(remote.temp.path(), &script);
    let client = GitClient::new(
      program,
      Duration::from_secs(60),
      vec![],
      format!("file://{}/", remote.temp.path().display()),
    );
    let store = remote.store();

    prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap_err();

    assert_eq!(
      fetch_count(&log),
      1,
      "network failures must not double the fetch work"
    );
  }

  #[test]
  fn option_shaped_paths_cannot_reach_git_as_options() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    // the parser rejects '-' components, so construct the source directly
    // to prove the -- separator holds on its own
    let canary = remote.temp.path().join("pwned.tar");
    let hostile = GitHubSkillRequest {
      key: "evil".into(),
      source: GitHubSource {
        owner: "owner".into(),
        repo: "repo".into(),
        path: Some(format!("-o{}", canary.display())),
      },
      r#ref: Some("main".into()),
      locked: None,
    };

    let error = prepare_github_skills(&client, &store, &[hostile]).unwrap_err();
    assert!(
      !canary.exists(),
      "an option-shaped path must never become a git option"
    );
    // behind --, git treats it as a pathspec that matches nothing
    assert!(format!("{error:#}").contains("failed to prepare skill 'evil'"));
  }

  #[test]
  fn locked_reconstruction_is_exact_or_cleaned_up() {
    let remote = skill_remote();
    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let prepared = prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap();
    let (commit, hash) = (&prepared[0].commit, &prepared[0].content_hash);

    // simulate a wiped cache, then rebuild the exact locked snapshot
    store.remove_snapshot(hash).unwrap();
    reconstruct_github_snapshot(
      &client,
      &store,
      &remote.source(Some("skills/skill-a")),
      commit,
      hash,
    )
    .unwrap();
    assert_eq!(store.verify_snapshot(hash).unwrap(), SnapshotStatus::Valid);

    // a hash that no longer reproduces is an error and leaves no orphan
    store.remove_snapshot(hash).unwrap();
    let wrong = format!("sha256:{}", "0".repeat(64));
    let error = reconstruct_github_snapshot(
      &client,
      &store,
      &remote.source(Some("skills/skill-a")),
      commit,
      &wrong,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no longer reproduces"));
    assert_eq!(
      store.verify_snapshot(hash).unwrap(),
      SnapshotStatus::Missing
    );
  }

  #[test]
  fn submodules_are_rejected() {
    let remote = skill_remote();

    // a second repo added as a submodule of the skill repo
    let sub = remote.temp.path().join("sub");
    fs::create_dir(&sub).unwrap();
    git(&sub, &["init", "-b", "main", "."]);
    fs::write(sub.join("f"), "x").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-m", "sub"]);

    git(
      &remote.work,
      &[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        sub.to_str().unwrap(),
        "vendored",
      ],
    );
    git(&remote.work, &["commit", "-m", "add submodule"]);
    git(
      &remote.work,
      &["push", remote.bare.to_str().unwrap(), "main:main"],
    );

    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let error =
      prepare_github_skills(&client, &store, &[remote.request("root-skill", None)]).unwrap_err();
    assert!(format!("{error:#}").contains("submodule"));
  }

  #[test]
  fn lfs_pointers_are_rejected_and_nothing_is_committed() {
    let remote = skill_remote();

    fs::write(
      remote.work.join("skills/skill-a/model.bin"),
      "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 5\n",
    )
    .unwrap();
    git(&remote.work, &["add", "."]);
    git(&remote.work, &["commit", "-m", "lfs pointer"]);
    git(
      &remote.work,
      &["push", remote.bare.to_str().unwrap(), "main:main"],
    );

    let log = remote.temp.path().join("git.log");
    let client = remote.logging_client(&log);
    let store = remote.store();

    let error = prepare_github_skills(
      &client,
      &store,
      &[remote.request("skill-a", Some("skills/skill-a"))],
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("LFS"));

    // a preparation failure must leave no committed snapshots behind
    let sha256 = remote.temp.path().join("store/sha256");
    let committed = fs::read_dir(&sha256).map(|dir| dir.count()).unwrap_or(0);
    assert_eq!(committed, 0);
  }

  #[test]
  #[cfg(unix)]
  fn local_object_reads_neutralize_global_attributes() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("env.log");
    let script = format!(r#"echo "$GIT_ATTR_NOSYSTEM $*" > "{}""#, log.display());
    let program = fake_git(temp.path(), &script);
    let client = fake_client(program, vec![]);

    client
      .run_attr_clean(&["ls-tree".to_string(), "HEAD".to_string()])
      .unwrap();

    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(
      logged.trim(),
      "1 -c core.attributesfile= -c core.autocrlf=false ls-tree HEAD"
    );
  }

  #[test]
  fn partial_phase_two_failures_clean_up_created_snapshots() {
    // mid-phase-two failures need environmental faults (disk full, perms),
    // so drive the phase directly: skill A commits, then skill B's tree
    // fails to materialize because its staging source vanished
    let temp = tempfile::tempdir().unwrap();
    let store = Store::new(&temp.path().join("store"));

    let dir_a = temp.path().join("a");
    write_skill_md(&dir_a, "skill-a");
    let tree_a = snapshot::scan_tree(&dir_a, GitFilter::IncludeAll).unwrap();

    let dir_b = temp.path().join("b");
    write_skill_md(&dir_b, "skill-b");
    let tree_b = snapshot::scan_tree(&dir_b, GitFilter::IncludeAll).unwrap();
    fs::remove_dir_all(&dir_b).unwrap();

    let sha = "0123456789abcdef0123456789abcdef01234567".to_string();
    let groups = vec![StagedGroup {
      skills: vec![
        StagedSkill {
          key: "skill-a".into(),
          commit: sha.clone(),
          outcome: StagedOutcome::Fresh { tree: tree_a },
        },
        StagedSkill {
          key: "skill-b".into(),
          commit: sha,
          outcome: StagedOutcome::Fresh { tree: tree_b },
        },
      ],
      _temp: None,
    }];

    commit_phase(&store, groups).unwrap_err();

    let leftovers = fs::read_dir(temp.path().join("store/sha256"))
      .map(|dir| dir.count())
      .unwrap_or(0);
    assert_eq!(
      leftovers, 0,
      "skill-a's committed snapshot must not survive the failed batch"
    );
  }

  #[test]
  fn timeout_parsing_and_base64() {
    assert_eq!(timeout_from(None), DEFAULT_TIMEOUT);
    assert_eq!(timeout_from(Some("42")), Duration::from_secs(42));
    assert_eq!(timeout_from(Some("0")), DEFAULT_TIMEOUT);
    assert_eq!(timeout_from(Some("nope")), DEFAULT_TIMEOUT);

    // RFC 4648 vectors
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
  }
}
