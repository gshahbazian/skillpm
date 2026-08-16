use std::path::PathBuf;

use anyhow::{Result, bail};

pub fn run(_source: String, _targets: Vec<PathBuf>, _ref: Option<String>) -> Result<()> {
  bail!("`spm add` is not implemented yet");
}
