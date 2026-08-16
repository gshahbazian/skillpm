mod cli;
mod commands;
mod config;
mod lockfile;
mod output;
mod paths;
mod platform;

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
