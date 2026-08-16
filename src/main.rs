mod archive;
mod cli;
mod commands;
mod config;
mod github;
mod local;
mod lockfile;
mod output;
mod paths;
mod platform;
mod skill;
mod snapshot;
mod source;
mod store;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

fn main() -> ExitCode {
  if let Err(error) = run() {
    output::error(&format!("{error:#}"));
    return ExitCode::FAILURE;
  }

  ExitCode::SUCCESS
}

fn run() -> Result<()> {
  platform::ensure_supported()?;

  let cli = cli::Cli::parse();
  commands::run(cli.command)
}
