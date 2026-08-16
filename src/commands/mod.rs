mod add;
mod install;
mod remove;
mod update;

use anyhow::Result;

use crate::cli::Command;

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
