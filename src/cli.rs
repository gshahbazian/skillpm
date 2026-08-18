use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

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
  // at least one destination is required, but either flag alone satisfies it
  #[command(group(
    ArgGroup::new("destination")
      .args(["targets", "agents"])
      .multiple(true)
      .required(true)
  ))]
  Add {
    /// GitHub source (github:owner/repo/path) or local path
    source: String,

    /// Path to link the skill into; repeat for multiple targets
    #[arg(long = "target", value_name = "PATH")]
    targets: Vec<PathBuf>,

    /// Agent skills directory to link the skill into; repeat for multiple agents
    #[arg(long = "agent", value_name = "AGENT", value_enum)]
    agents: Vec<AgentDir>,

    /// Branch, tag, or full commit SHA; GitHub sources only
    #[arg(long = "ref", value_name = "REF")]
    r#ref: Option<String>,
  },

  /// Unlink a skill's targets and remove it from skillpm.toml
  Remove { name: String },
}

/// A well-known agent skills directory. `--agent` is shorthand for a
/// `--target` under one of these roots; the skill name is appended once it is
/// known, so the shorthand cannot produce a mismatched target basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentDir {
  Agents,
  Pi,
  Claude,
}

impl AgentDir {
  /// The skills root in the `~/` spelling written verbatim into skillpm.toml.
  pub fn skills_root(self) -> &'static str {
    match self {
      Self::Agents => "~/.agents/skills",
      Self::Pi => "~/.pi/agent/skills",
      Self::Claude => "~/.claude/skills",
    }
  }
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
        agents: Vec::new(),
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
  fn add_parses_each_agent_value() {
    for (value, expected) in [
      ("agents", AgentDir::Agents),
      ("pi", AgentDir::Pi),
      ("claude", AgentDir::Claude),
    ] {
      let command = parse(&["skillpm", "add", "skills/x", "--agent", value]).unwrap();
      let Command::Add {
        targets, agents, ..
      } = command
      else {
        panic!("expected add");
      };
      assert!(targets.is_empty());
      assert_eq!(agents, vec![expected]);
    }
  }

  #[test]
  fn add_parses_repeated_agents() {
    let command = parse(&[
      "skillpm", "add", "skills/x", "--agent", "claude", "--agent", "pi",
    ])
    .unwrap();

    let Command::Add { agents, .. } = command else {
      panic!("expected add");
    };
    assert_eq!(agents, vec![AgentDir::Claude, AgentDir::Pi]);
  }

  #[test]
  fn add_parses_agents_combined_with_targets() {
    let command = parse(&[
      "skillpm", "add", "skills/x", "--target", "links/x", "--agent", "agents",
    ])
    .unwrap();

    let Command::Add {
      targets, agents, ..
    } = command
    else {
      panic!("expected add");
    };
    assert_eq!(targets, vec![PathBuf::from("links/x")]);
    assert_eq!(agents, vec![AgentDir::Agents]);
  }

  #[test]
  fn add_rejects_unknown_and_uppercase_agent_values() {
    // lowercase spellings only; the error lists the valid values
    let error = parse(&["skillpm", "add", "skills/x", "--agent", "CLAUDE"]).unwrap_err();
    let message = error.to_string();
    assert!(
      message.contains("claude"),
      "expected valid values: {message}"
    );
    assert!(parse(&["skillpm", "add", "skills/x", "--agent", "cursor"]).is_err());
  }

  #[test]
  fn add_rejects_a_bare_agents_flag() {
    assert!(parse(&["skillpm", "add", "skills/x", "--agent"]).is_err());
  }

  #[test]
  fn agent_values_map_to_skills_roots() {
    assert_eq!(AgentDir::Agents.skills_root(), "~/.agents/skills");
    assert_eq!(AgentDir::Pi.skills_root(), "~/.pi/agent/skills");
    assert_eq!(AgentDir::Claude.skills_root(), "~/.claude/skills");
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
  fn add_requires_a_target_or_an_agent() {
    let error = parse(&["skillpm", "add", "github:anthropics/skills/frontend-design"]).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("--target"), "expected usage: {message}");
    assert!(message.contains("--agent"), "expected usage: {message}");
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
