use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  Install,

  Add {
    source: String,

    #[arg(short, long)]
    target: Vec<PathBuf>,
  },

  Remove {
    name: String,
  },
}

fn main() -> Result<()> {
  let cli = Cli::parse();

  match cli.command {
    Command::Install => install(),
    Command::Add { source, target } => add(source, target),
    Command::Remove { name } => remove(name),
  }
}

fn install() -> Result<()> {
  println!("installing skills");
  Ok(())
}

fn add(source: String, targets: Vec<PathBuf>) -> Result<()> {
  println!("adding {source}");

  for target in targets {
    println!("  -> {}", target.display());
  }

  Ok(())
}

fn remove(name: String) -> Result<()> {
  println!("removing {name}");
  Ok(())
}
