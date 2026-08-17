use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "skillpm", version, about = "Declarative skill package manager")]
pub struct Cli {
  #[command(subcommand)]
  pub command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
  /// Reproduce skillpm.lock exactly, without resolving new versions
  Install,

  /// Resolve new versions and regenerate the lockfile
  Update,

  /// Fetch a source, add it to skillpm.toml, and install its targets
  Add {
    /// GitHub source (github:owner/repo/path) or local path
    source: String,

    /// Path to link the skill into; repeat for multiple targets
    #[arg(long = "target", value_name = "PATH", required = true)]
    targets: Vec<PathBuf>,

    /// Branch, tag, or full commit SHA; GitHub sources only
    #[arg(long = "ref", value_name = "REF")]
    r#ref: Option<String>,
  },

  /// Unlink a skill's targets and remove it from skillpm.toml
  Remove { name: String },
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(args: &[&str]) -> Result<Command, clap::Error> {
    Cli::try_parse_from(args).map(|cli| cli.command)
  }

  #[test]
  fn install_parses() {
    assert_eq!(parse(&["skillpm", "install"]).unwrap(), Command::Install);
  }

  #[test]
  fn update_parses() {
    assert_eq!(parse(&["skillpm", "update"]).unwrap(), Command::Update);
  }

  #[test]
  fn add_parses_source_and_target() {
    let command = parse(&[
      "skillpm",
      "add",
      "github:anthropics/skills/frontend-design",
      "--target",
      ".claude/skills/frontend-design",
    ])
    .unwrap();

    assert_eq!(
      command,
      Command::Add {
        source: "github:anthropics/skills/frontend-design".into(),
        targets: vec![PathBuf::from(".claude/skills/frontend-design")],
        r#ref: None,
      }
    );
  }

  #[test]
  fn add_parses_repeated_targets() {
    let command = parse(&[
      "skillpm",
      "add",
      "github:anthropics/skills/frontend-design",
      "--target",
      ".claude/skills/frontend-design",
      "--target",
      ".agents/skills/frontend-design",
    ])
    .unwrap();

    let Command::Add { targets, .. } = command else {
      panic!("expected add");
    };
    assert_eq!(
      targets,
      vec![
        PathBuf::from(".claude/skills/frontend-design"),
        PathBuf::from(".agents/skills/frontend-design"),
      ]
    );
  }

  #[test]
  fn add_parses_ref() {
    let command = parse(&[
      "skillpm",
      "add",
      "github:anthropics/skills/frontend-design",
      "--target",
      ".claude/skills/frontend-design",
      "--ref",
      "main",
    ])
    .unwrap();

    let Command::Add { r#ref, .. } = command else {
      panic!("expected add");
    };
    assert_eq!(r#ref, Some("main".into()));
  }

  #[test]
  fn add_parses_ref_for_local_source() {
    // ref-vs-source validation belongs to the source layer, not the parser
    let command = parse(&[
      "skillpm",
      "add",
      "skills/my-local-skill",
      "--target",
      ".claude/skills/my-local-skill",
      "--ref",
      "main",
    ])
    .unwrap();

    let Command::Add { r#ref, .. } = command else {
      panic!("expected add");
    };
    assert_eq!(r#ref, Some("main".into()));
  }

  #[test]
  fn remove_parses_name() {
    assert_eq!(
      parse(&["skillpm", "remove", "frontend-design"]).unwrap(),
      Command::Remove {
        name: "frontend-design".into(),
      }
    );
  }

  #[test]
  fn bare_invocation_is_an_error() {
    assert!(parse(&["skillpm"]).is_err());
  }

  #[test]
  fn unknown_subcommand_is_an_error() {
    assert!(parse(&["skillpm", "sync"]).is_err());
  }

  #[test]
  fn add_requires_a_source() {
    assert!(parse(&["skillpm", "add", "--target", ".claude/skills/x"]).is_err());
  }

  #[test]
  fn add_requires_at_least_one_target() {
    assert!(parse(&["skillpm", "add", "github:anthropics/skills/frontend-design"]).is_err());
  }

  #[test]
  fn add_rejects_a_bare_ref_flag() {
    assert!(
      parse(&[
        "skillpm",
        "add",
        "github:a/b",
        "--target",
        ".claude/skills/b",
        "--ref"
      ])
      .is_err()
    );
  }

  #[test]
  fn remove_requires_a_name() {
    assert!(parse(&["skillpm", "remove"]).is_err());
  }

  #[test]
  fn remove_rejects_extra_names() {
    assert!(parse(&["skillpm", "remove", "one", "two"]).is_err());
  }

  #[test]
  fn install_rejects_selective_arguments() {
    assert!(parse(&["skillpm", "install", "frontend-design"]).is_err());
  }

  #[test]
  fn update_rejects_selective_arguments() {
    assert!(parse(&["skillpm", "update", "frontend-design"]).is_err());
  }

  #[test]
  fn unsupported_flags_are_rejected() {
    // v1 has no config override, force, JSON, quiet, or prompt options
    assert!(parse(&["skillpm", "--config", "skillpm.toml", "install"]).is_err());
    assert!(parse(&["skillpm", "install", "--json"]).is_err());
    assert!(parse(&["skillpm", "update", "--quiet"]).is_err());
    assert!(parse(&["skillpm", "add", "skills/x", "--target", "t/x", "--force"]).is_err());
    assert!(parse(&["skillpm", "remove", "x", "--force"]).is_err());
    assert!(parse(&["skillpm", "install", "--ref", "main"]).is_err());
  }
}
