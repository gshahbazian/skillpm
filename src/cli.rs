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
  fn simple_commands_parse() {
    let cases: &[(&[&str], Command)] = &[
      (&["skillpm", "install"], Command::Install),
      (&["skillpm", "update"], Command::Update),
      (
        &["skillpm", "remove", "frontend-design"],
        Command::Remove {
          name: "frontend-design".into(),
        },
      ),
    ];

    for (args, expected) in cases {
      assert_eq!(&parse(args).unwrap(), expected, "failed to parse {args:?}");
    }
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
  fn value_flags_require_values() {
    for args in [
      &["skillpm", "add", "skills/x", "--agent"][..],
      &[
        "skillpm",
        "add",
        "github:a/b",
        "--target",
        ".claude/skills/b",
        "--ref",
      ],
    ] {
      assert!(parse(args).is_err(), "expected an error for {args:?}");
    }
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
  fn add_requires_a_target_or_an_agent() {
    let error = parse(&["skillpm", "add", "github:anthropics/skills/frontend-design"]).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("--target"), "expected usage: {message}");
    assert!(message.contains("--agent"), "expected usage: {message}");
  }

  #[test]
  fn invalid_command_shapes_are_rejected() {
    for args in [
      &["skillpm"][..],
      &["skillpm", "sync"],
      &["skillpm", "add", "--target", ".claude/skills/x"],
      &["skillpm", "remove"],
      &["skillpm", "remove", "one", "two"],
      &["skillpm", "install", "frontend-design"],
      &["skillpm", "update", "frontend-design"],
    ] {
      assert!(parse(args).is_err(), "expected an error for {args:?}");
    }
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
