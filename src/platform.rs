use anyhow::{Result, bail};

/// v1 never falls back to copied targets, so anything but macOS/Linux is a hard error.
pub fn ensure_supported() -> Result<()> {
  if cfg!(target_os = "windows") {
    bail!("Windows is not supported; spm requires macOS or Linux");
  }

  if !cfg!(any(target_os = "macos", target_os = "linux")) {
    bail!(
      "unsupported platform '{}'; spm requires macOS or Linux",
      std::env::consts::OS
    );
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  fn supported_platforms_pass_the_guard() {
    assert!(ensure_supported().is_ok());
  }

  #[test]
  #[cfg(target_os = "windows")]
  fn windows_fails_the_guard() {
    let error = ensure_supported().unwrap_err();
    assert!(error.to_string().contains("Windows is not supported"));
  }
}
