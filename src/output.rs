use std::io::IsTerminal;

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Progress lines go to stderr so stdout stays a clean success summary.
pub fn progress(message: &str) {
  eprintln!("{message}");
}

pub fn warning(message: &str) {
  eprintln!("{}{message}", paint(YELLOW, "warning: "));
}

pub fn error(message: &str) {
  eprintln!("{}{message}", paint(RED, "error: "));
}

/// The one thing that belongs on stdout.
pub fn success(message: &str) {
  println!("{message}");
}

fn paint(color: &str, text: &str) -> String {
  if !stderr_color_enabled() {
    return text.to_string();
  }

  format!("{color}{text}{RESET}")
}

fn stderr_color_enabled() -> bool {
  let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
  color_enabled(std::io::stderr().is_terminal(), no_color)
}

fn color_enabled(stderr_is_tty: bool, no_color: bool) -> bool {
  if no_color {
    return false;
  }

  stderr_is_tty
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn color_requires_a_tty() {
    assert!(color_enabled(true, false));
    assert!(!color_enabled(false, false));
  }

  #[test]
  fn no_color_wins_even_on_a_tty() {
    assert!(!color_enabled(true, true));
    assert!(!color_enabled(false, true));
  }
}
