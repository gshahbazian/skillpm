use std::process::{Command, Output};

/// Every invocation runs against an isolated empty HOME so tests can never
/// read or touch the developer's real spm state.
fn spm_in(home: &std::path::Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_spm"))
    .args(args)
    .env("NO_COLOR", "1")
    .env("HOME", home)
    .env_remove("XDG_CONFIG_HOME")
    .env_remove("XDG_DATA_HOME")
    .output()
    .expect("failed to run spm")
}

fn spm(args: &[&str]) -> Output {
  let home = tempfile::tempdir().expect("failed to create temp home");
  spm_in(home.path(), args)
}

#[test]
fn stub_commands_fail_with_errors_on_stderr_only() {
  // `remove` is the last remaining stub
  let output = spm(&["remove", "x"]);

  assert!(!output.status.success());
  assert!(output.stdout.is_empty(), "stdout must stay clean");

  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.contains("error: "), "missing error prefix: {stderr}");
  assert!(
    stderr.contains("not implemented"),
    "unexpected stderr: {stderr}"
  );
}

#[test]
fn install_on_a_fresh_home_reports_missing_setup() {
  let output = spm(&["install"]);

  assert!(!output.status.success());
  assert!(output.stdout.is_empty());

  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.contains("error: "), "missing error prefix: {stderr}");
  assert!(
    stderr.contains("run `spm add`"),
    "expected the bootstrap hint: {stderr}"
  );
}

#[test]
fn no_color_strips_ansi_from_stderr() {
  let output = spm(&["install"]);
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(
    !stderr.contains('\x1b'),
    "found ANSI escapes despite NO_COLOR: {stderr:?}"
  );
}

#[test]
fn usage_errors_report_on_stderr_with_a_failing_status() {
  let output = spm(&["add", "skills/x"]);

  assert!(!output.status.success());
  assert!(output.stdout.is_empty());

  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(
    stderr.contains("--target"),
    "expected usage mentioning --target: {stderr}"
  );
}

#[test]
fn help_and_version_are_available() {
  assert!(spm(&["--help"]).status.success());
  assert!(spm(&["--version"]).status.success());
}
