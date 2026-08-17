use std::path::PathBuf;

use anyhow::{Result, bail};

/// The two supported source forms. Anything that is not `github:` is a local
/// path; bare `owner/repo` shorthand is intentionally a local path, not GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
  GitHub(GitHubSource),
  Local(LocalSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSource {
  pub owner: String,
  pub repo: String,
  /// Skill directory inside the repository; None selects the repository root.
  pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSource {
  /// As authored; resolution happens in the paths layer.
  pub path: PathBuf,
}

pub fn parse_source(input: &str) -> Result<Source> {
  if input.is_empty() {
    bail!("source must not be empty");
  }

  if let Some(rest) = input.strip_prefix("github:") {
    return parse_github(input, rest).map(Source::GitHub);
  }

  // catch the URL and SSH spellings people will inevitably try
  if input.contains("://") || input.starts_with("git@") || input.starts_with("github.com/") {
    bail!("'{input}' is not a supported source; use github:<owner>/<repo>/<optional-path>");
  }

  Ok(Source::Local(LocalSource {
    path: PathBuf::from(input),
  }))
}

fn parse_github(input: &str, rest: &str) -> Result<GitHubSource> {
  let invalid = |reason: &str| -> anyhow::Error {
    anyhow::anyhow!("invalid GitHub source '{input}': {reason}")
  };

  if rest.contains('?') || rest.contains('#') {
    return Err(invalid("query strings and fragments are not supported"));
  }

  let components: Vec<&str> = rest.split('/').collect();
  if components.len() < 2 {
    return Err(invalid("expected github:<owner>/<repo>/<optional-path>"));
  }

  for component in &components {
    if component.is_empty() {
      return Err(invalid("empty path component"));
    }
    if *component == "." || *component == ".." {
      return Err(invalid("path traversal is not supported"));
    }
    if component.contains('\\') || component.chars().any(char::is_whitespace) {
      return Err(invalid("unsupported character in component"));
    }
    // a leading '-' could read as an option to a git subcommand
    if component.starts_with('-') {
      return Err(invalid("component may not start with '-'"));
    }
  }

  let path = if components.len() > 2 {
    Some(components[2..].join("/"))
  } else {
    None
  };

  Ok(GitHubSource {
    owner: components[0].to_string(),
    repo: components[1].to_string(),
    path,
  })
}

impl Source {
  /// `ref` lives outside the source string but is only meaningful for GitHub.
  pub fn validate_ref(&self, r#ref: Option<&str>) -> Result<()> {
    let Some(r) = r#ref else {
      return Ok(());
    };

    if matches!(self, Source::Local(_)) {
      bail!("'ref' is only valid for GitHub sources");
    }
    if r.is_empty() {
      bail!("'ref' must not be empty");
    }
    // a ref starting with '-' could be read as a git argument
    if r.starts_with('-') {
      bail!("invalid ref '{r}'");
    }
    if r.chars().any(char::is_whitespace) {
      bail!("invalid ref '{r}': whitespace is not allowed");
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn github(owner: &str, repo: &str, path: Option<&str>) -> Source {
    Source::GitHub(GitHubSource {
      owner: owner.into(),
      repo: repo.into(),
      path: path.map(String::from),
    })
  }

  fn local(path: &str) -> Source {
    Source::Local(LocalSource { path: path.into() })
  }

  #[test]
  fn valid_sources_parse() {
    let cases: &[(&str, Source)] = &[
      // repository-root skill
      (
        "github:anthropics/skills",
        github("anthropics", "skills", None),
      ),
      (
        "github:anthropics/skills/frontend-design",
        github("anthropics", "skills", Some("frontend-design")),
      ),
      // nested skill path
      (
        "github:a/b/deep/nested/skill",
        github("a", "b", Some("deep/nested/skill")),
      ),
      ("/opt/skills/x", local("/opt/skills/x")),
      ("skills/my-local-skill", local("skills/my-local-skill")),
      ("~/skills/x", local("~/skills/x")),
      // bare owner/repo shorthand is a local path by design
      ("anthropics/skills", local("anthropics/skills")),
    ];

    for (input, expected) in cases {
      assert_eq!(&parse_source(input).unwrap(), expected, "input: {input}");
    }
  }

  #[test]
  fn malformed_sources_are_rejected() {
    let cases: &[&str] = &[
      "",
      "github:",
      "github:owner",
      "github:owner/",
      "github:/repo",
      "github:owner//path",
      "github:owner/repo/",
      "github:owner/repo/../escape",
      "github:owner/repo/./x",
      "github:owner/repo/path?query=1",
      "github:owner/repo#fragment",
      "github:owner/repo/pa th",
      "github:owner/repo/pa\\th",
      "github:owner/repo/-opath",
      "github:owner/-flag/path",
      "https://github.com/anthropics/skills",
      "git@github.com:anthropics/skills.git",
      "github.com/anthropics/skills",
    ];

    for input in cases {
      assert!(parse_source(input).is_err(), "expected rejection: {input}");
    }
  }

  #[test]
  fn refs_are_rejected_on_local_sources() {
    let source = parse_source("skills/x").unwrap();
    assert!(source.validate_ref(None).is_ok());

    let error = source.validate_ref(Some("main")).unwrap_err();
    assert!(error.to_string().contains("only valid for GitHub"));
  }

  #[test]
  fn github_refs_are_sanity_checked() {
    let source = parse_source("github:a/b").unwrap();

    for valid in [
      "main",
      "v1.2.3",
      "0123456789abcdef0123456789abcdef01234567",
      "feature/nested-branch",
    ] {
      assert!(source.validate_ref(Some(valid)).is_ok(), "ref: {valid}");
    }

    for invalid in ["", "-rf", "my branch", "tab\tref"] {
      assert!(
        source.validate_ref(Some(invalid)).is_err(),
        "ref: {invalid:?}"
      );
    }
  }
}
