use std::process::{Command, Output};

fn spm(args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_spm"))
    .args(args)
    .env("NO_COLOR", "1")
    .output()
    .expect("failed to run spm")
}

#[test]
fn stub_commands_fail_with_errors_on_stderr_only() {
  for args in [
    vec!["install"],
    vec!["update"],
    vec!["add", "skills/x", "--target", ".claude/skills/x"],
    vec!["remove", "x"],
  ] {
    let output = spm(&args);

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
      stderr.contains("not implemented"),
      "unexpected stderr for {args:?}: {stderr}"
    );
  }
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
