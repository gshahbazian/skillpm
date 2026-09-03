use anyhow::{Result, bail};

/// v1 never falls back to copied targets, so anything but macOS/Linux is a hard error.
pub fn ensure_supported() -> Result<()> {
  if cfg!(target_os = "windows") {
    bail!("Windows is not supported; skillpm requires macOS or Linux");
  }

  if !cfg!(any(target_os = "macos", target_os = "linux")) {
    bail!(
      "unsupported platform '{}'; skillpm requires macOS or Linux",
      std::env::consts::OS
    );
  }

  Ok(())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use super::*;

  #[test]
  fn windows_fails_the_guard() {
    let error = ensure_supported().unwrap_err();
    assert!(error.to_string().contains("Windows is not supported"));
  }
}
