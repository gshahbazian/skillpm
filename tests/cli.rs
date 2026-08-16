use std::process::{Command, Output};

/// Every invocation runs against an isolated empty HOME so tests can never
/// read or touch the developer's real skillpm state.
fn skillpm_in(home: &std::path::Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_skillpm"))
    .args(args)
    .env("NO_COLOR", "1")
    .env("HOME", home)
    .env_remove("XDG_CONFIG_HOME")
    .env_remove("XDG_DATA_HOME")
    .output()
    .expect("failed to run skillpm")
}

fn skillpm(args: &[&str]) -> Output {
  let home = tempfile::tempdir().expect("failed to create temp home");
  skillpm_in(home.path(), args)
}

#[test]
fn commands_on_a_fresh_home_fail_with_errors_on_stderr_only() {
  for args in [vec!["update"], vec!["remove", "x"]] {
    let output = skillpm(&args);

    assert!(!output.status.success(), "expected failure for {args:?}");
    assert!(
      output.stdout.is_empty(),
      "stdout must stay clean for {args:?}"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
      stderr.contains("error: "),
      "missing error prefix for {args:?}: {stderr}"
    );
    assert!(
      stderr.contains("run `skillpm add`"),
      "expected the bootstrap hint for {args:?}: {stderr}"
    );
  }
}

#[test]
fn install_on_a_fresh_home_reports_missing_setup() {
  let output = skillpm(&["install"]);

  assert!(!output.status.success());
  assert!(output.stdout.is_empty());

  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.contains("error: "), "missing error prefix: {stderr}");
  assert!(
    stderr.contains("run `skillpm add`"),
    "expected the bootstrap hint: {stderr}"
  );
}

#[test]
fn no_color_strips_ansi_from_stderr() {
  let output = skillpm(&["install"]);
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(
    !stderr.contains('\x1b'),
    "found ANSI escapes despite NO_COLOR: {stderr:?}"
  );
}

#[test]
fn usage_errors_report_on_stderr_with_a_failing_status() {
  let output = skillpm(&["add", "skills/x"]);

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
  assert!(skillpm(&["--help"]).status.success());
  assert!(skillpm(&["--version"]).status.success());
}
