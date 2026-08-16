#![allow(dead_code)] // consumed by the command tickets

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::source::GitHubSource;

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
      timeout: timeout_from(std::env::var("SPM_GIT_TIMEOUT_SECONDS").ok().as_deref()),
      tokens,
      remote_base: "https://github.com/".to_string(),
    }
  }

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
    let mut attempts: Vec<Option<&str>> = vec![None];
    attempts.extend(self.tokens.iter().map(|token| Some(token.as_str())));

    let mut last_error = String::new();
    for token in attempts {
      let output = self.run_once(args, token)?;
      if output.success {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
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

  fn run_once(&self, args: &[String], token: Option<&str>) -> Result<RawOutput> {
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
          "git executable '{}' was not found; spm requires the system git",
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
          "git timed out after {} seconds (override with SPM_GIT_TIMEOUT_SECONDS)",
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

/// Kills git and every descendant in its process group; a lone child.kill()
/// leaves helper children alive holding the output pipes, which would hang
/// the reader threads past the timeout.
fn kill_process_group(child: &mut std::process::Child) {
  #[cfg(unix)]
  {
    // the child was spawned with process_group(0), so its pid is the pgid;
    // the negative pid targets the whole group
    #[allow(unsafe_code)] // no safe std API for killpg
    unsafe {
      libc::kill(-(child.id() as i32), libc::SIGKILL);
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

  fn fake_client(program: PathBuf, tokens: Vec<String>) -> GitClient {
    GitClient::new(
      program,
      Duration::from_millis(400),
      tokens,
      "https://github.com/".into(),
    )
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
    assert!(error.to_string().contains("spm requires the system git"));
  }

  #[test]
  #[cfg(unix)]
  fn timeouts_kill_the_child() {
    let temp = tempfile::tempdir().unwrap();
    let program = fake_git(temp.path(), "sleep 30");
    let client = fake_client(program, vec![]);
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
    let client = fake_client(program, vec![]);
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
