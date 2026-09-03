use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The single global runtime layout. No project-directory lookup, no overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
  pub home: PathBuf,
  pub config_file: PathBuf,
  pub lockfile: PathBuf,
  pub data_root: PathBuf,
  pub store: PathBuf,
  pub operation_lock: PathBuf,
}

impl Paths {
  pub fn from_env() -> Result<Self> {
    let home = home_dir()?;
    Ok(Self::new(
      &home,
      std::env::var_os("XDG_CONFIG_HOME").as_deref(),
      std::env::var_os("XDG_DATA_HOME").as_deref(),
    ))
  }

  fn new(home: &Path, xdg_config: Option<&OsStr>, xdg_data: Option<&OsStr>) -> Self {
    let config_dir = base_dir(home, xdg_config, &[".config"]).join("skillpm");
    let data_root = base_dir(home, xdg_data, &[".local", "share"]).join("skillpm");

    Self {
      home: home.to_path_buf(),
      config_file: config_dir.join("skillpm.toml"),
      lockfile: config_dir.join("skillpm.lock"),
      store: data_root.join("store"),
      operation_lock: data_root.join(".operation.lock"),
      data_root,
    }
  }

  pub fn config_dir(&self) -> &Path {
    self.config_file.parent().expect("config file has a parent")
  }

  /// Only `add` bootstraps; existing directories keep whatever permissions they have.
  pub fn create_runtime_dirs(&self) -> Result<()> {
    for dir in [self.config_dir(), &self.data_root, &self.store] {
      create_private_dir(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(())
  }
}

/// Per the XDG spec, relative override values are ignored.
fn base_dir(home: &Path, xdg_override: Option<&OsStr>, default_parts: &[&str]) -> PathBuf {
  if let Some(value) = xdg_override
    && Path::new(value).is_absolute()
  {
    return PathBuf::from(value);
  }

  let mut dir = home.to_path_buf();
  for part in default_parts {
    dir.push(part);
  }
  dir
}

fn home_dir() -> Result<PathBuf> {
  let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
    bail!("HOME is not set");
  };

  let home = PathBuf::from(home);
  if !home.is_absolute() {
    bail!("HOME must be an absolute path, got '{}'", home.display());
  }

  Ok(home)
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
  let mut builder = fs::DirBuilder::new();
  builder.recursive(true);

  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }

  builder.create(path)
}

/// Resolves a user-supplied local source or target path to an absolute path.
///
/// Accepts absolute paths, home-relative paths, and a leading `~/`. Never
/// expands environment variables, globs, or non-leading tildes; `$FOO` is a
/// literal component.
pub fn resolve_user_path(input: &Path, home: &Path) -> Result<PathBuf> {
  if input.as_os_str().is_empty() {
    bail!("path is empty");
  }

  if input.is_absolute() {
    return Ok(normalize(input));
  }

  let mut components = input.components();
  let first = components.next();

  if let Some(Component::Normal(first)) = first
    && first.as_encoded_bytes().starts_with(b"~")
  {
    // bare "~" is rejected too; the contract allows only the ~/ spelling
    if first != "~" || input == Path::new("~") {
      bail!(
        "unsupported tilde expansion in '{}'; only a leading ~/ is supported",
        input.display()
      );
    }
    return Ok(normalize(&home.join(components.as_path())));
  }

  Ok(normalize(&home.join(input)))
}

/// Drops `.` components; `Components` already collapses repeated separators.
/// `..` is kept for `canonicalize_existing_prefix` to resolve against the
/// real filesystem instead of guessing lexically past symlinks.
fn normalize(path: &Path) -> PathBuf {
  path
    .components()
    .filter(|component| !matches!(component, Component::CurDir))
    .collect()
}

/// Follows a symlink chain even when the final target does not exist yet,
/// which plain canonicalize reports as NotFound. Writing through the result
/// (instead of the given path) is what keeps a symlinked file a symlink.
pub fn resolve_real_path(path: &Path) -> Result<PathBuf> {
  let mut current = path.to_path_buf();

  // 40 matches the Linux kernel's symlink-following limit
  for _ in 0..40 {
    let Ok(destination) = fs::read_link(&current) else {
      // not a symlink (or nothing there at all)
      return canonicalize_existing_prefix(&current);
    };

    current = if destination.is_absolute() {
      destination
    } else {
      current
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(destination)
    };
  }

  bail!("too many levels of symbolic links at {}", path.display());
}

/// Canonicalizes the deepest existing ancestor (following symlinked parents)
/// and reattaches the not-yet-created tail. Used for conflict detection while
/// the configured spelling stays untouched.
pub fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
  if !path.is_absolute() {
    bail!("cannot canonicalize relative path '{}'", path.display());
  }

  let mut current = path.to_path_buf();
  let mut tail: Vec<std::ffi::OsString> = Vec::new();

  loop {
    match current.canonicalize() {
      Ok(real) => {
        let mut out = real;
        for component in tail.iter().rev() {
          out.push(component);
        }
        return Ok(out);
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        let Some(name) = current.file_name() else {
          bail!(
            "cannot resolve '{}'; a missing component is followed by '..'",
            path.display()
          );
        };
        tail.push(name.to_os_string());
        current = current
          .parent()
          .expect("named path has a parent")
          .to_path_buf();
      }
      Err(error) => {
        return Err(error).with_context(|| format!("failed to canonicalize {}", current.display()));
      }
    }
  }
}

/// Held for the duration of a mutating command; the flock drops with the file.
/// The lock file itself is left in place to avoid unlink races.
#[derive(Debug)]
pub struct OperationLock {
  _file: File,
}

impl OperationLock {
  /// Nonblocking: fails immediately if another skillpm process holds the lock.
  pub fn acquire(lock_path: &Path) -> Result<Self> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(false);

    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }

    let file = options
      .open(lock_path)
      .with_context(|| format!("failed to open operation lock {}", lock_path.display()))?;

    match file.try_lock() {
      Ok(()) => Ok(Self { _file: file }),
      Err(TryLockError::WouldBlock) => bail!(
        "another skillpm process is already running (operation lock {} is held)",
        lock_path.display()
      ),
      Err(TryLockError::Error(error)) => {
        Err(error).with_context(|| format!("failed to lock {}", lock_path.display()))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn home() -> PathBuf {
    PathBuf::from("/home/tester")
  }

  #[test]
  fn default_layout_lives_under_home() {
    let paths = Paths::new(&home(), None, None);

    assert_eq!(
      paths.config_file,
      home().join(".config/skillpm/skillpm.toml")
    );
    assert_eq!(paths.lockfile, home().join(".config/skillpm/skillpm.lock"));
    assert_eq!(paths.data_root, home().join(".local/share/skillpm"));
    assert_eq!(paths.store, home().join(".local/share/skillpm/store"));
    assert_eq!(
      paths.operation_lock,
      home().join(".local/share/skillpm/.operation.lock")
    );
  }

  #[test]
  fn xdg_overrides_are_honored() {
    let paths = Paths::new(
      &home(),
      Some(OsStr::new("/xdg/config")),
      Some(OsStr::new("/xdg/data")),
    );

    assert_eq!(
      paths.config_file,
      PathBuf::from("/xdg/config/skillpm/skillpm.toml")
    );
    assert_eq!(
      paths.lockfile,
      PathBuf::from("/xdg/config/skillpm/skillpm.lock")
    );
    assert_eq!(paths.data_root, PathBuf::from("/xdg/data/skillpm"));
    assert_eq!(paths.store, PathBuf::from("/xdg/data/skillpm/store"));
    assert_eq!(
      paths.operation_lock,
      PathBuf::from("/xdg/data/skillpm/.operation.lock")
    );
  }

  #[test]
  fn relative_or_empty_xdg_values_fall_back_to_defaults() {
    let relative = Paths::new(&home(), Some(OsStr::new("cfg")), Some(OsStr::new("data")));
    assert_eq!(
      relative.config_file,
      home().join(".config/skillpm/skillpm.toml")
    );
    assert_eq!(relative.data_root, home().join(".local/share/skillpm"));

    let empty = Paths::new(&home(), Some(OsStr::new("")), Some(OsStr::new("")));
    assert_eq!(
      empty.config_file,
      home().join(".config/skillpm/skillpm.toml")
    );
    assert_eq!(empty.data_root, home().join(".local/share/skillpm"));
  }

  #[test]
  fn paths_resolve_without_expanding_literals() {
    let cases = [
      ("/opt/skills/x", PathBuf::from("/opt/skills/x")),
      ("skills/x", home().join("skills/x")),
      ("$HOME/skills", home().join("$HOME/skills")),
      ("skills/~x", home().join("skills/~x")),
    ];

    for (input, expected) in cases {
      assert_eq!(
        resolve_user_path(Path::new(input), &home()).unwrap(),
        expected
      );
    }
  }

  #[test]
  fn leading_tilde_slash_resolves_to_home() {
    let resolved = resolve_user_path(Path::new("~/skills/x"), &home()).unwrap();
    assert_eq!(resolved, home().join("skills/x"));
  }

  #[test]
  fn bare_tilde_is_rejected() {
    let error = resolve_user_path(Path::new("~"), &home()).unwrap_err();
    assert!(error.to_string().contains("tilde"));
  }

  #[test]
  fn user_tilde_expansion_is_rejected() {
    let error = resolve_user_path(Path::new("~other/skills"), &home()).unwrap_err();
    assert!(error.to_string().contains("tilde"));
  }

  #[test]
  fn empty_paths_are_rejected() {
    assert!(resolve_user_path(Path::new(""), &home()).is_err());
  }

  #[test]
  fn dots_and_repeated_separators_normalize_away() {
    let resolved = resolve_user_path(Path::new("./skills//./x/"), &home()).unwrap();
    assert_eq!(resolved, home().join("skills/x"));

    let absolute = resolve_user_path(Path::new("/opt//skills/./x"), &home()).unwrap();
    assert_eq!(absolute, PathBuf::from("/opt/skills/x"));
  }

  #[test]
  #[cfg(unix)]
  fn canonicalize_resolves_symlinked_existing_parents() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();

    std::os::unix::fs::symlink(&real, temp.path().join("link")).unwrap();

    let input = temp.path().join("link/new-dir/skill");
    let resolved = canonicalize_existing_prefix(&input).unwrap();

    // macOS tempdirs sit behind /var -> /private/var, so compare canonical to canonical
    assert_eq!(resolved, real.canonicalize().unwrap().join("new-dir/skill"));
  }

  #[test]
  fn canonicalize_keeps_fully_missing_tails() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("a/b/c");

    let resolved = canonicalize_existing_prefix(&input).unwrap();
    assert_eq!(resolved, temp.path().canonicalize().unwrap().join("a/b/c"));
  }

  #[test]
  fn canonicalize_rejects_relative_paths() {
    assert!(canonicalize_existing_prefix(Path::new("skills/x")).is_err());
  }

  #[test]
  fn operation_lock_excludes_a_second_holder() {
    let temp = tempfile::tempdir().unwrap();
    let lock_path = temp.path().join(".operation.lock");

    let held = OperationLock::acquire(&lock_path).unwrap();

    // flock treats separately opened descriptors independently, so this
    // models a second process
    let error = OperationLock::acquire(&lock_path).unwrap_err();
    assert!(error.to_string().contains("another skillpm process"));

    drop(held);

    // a concurrently forked test subprocess can briefly hold an inherited
    // duplicate of the descriptor, keeping the flock alive past our drop;
    // retry for a moment instead of flaking
    let mut reacquired = OperationLock::acquire(&lock_path);
    for _ in 0..100 {
      if reacquired.is_ok() {
        break;
      }
      std::thread::sleep(std::time::Duration::from_millis(10));
      reacquired = OperationLock::acquire(&lock_path);
    }
    reacquired.unwrap();
  }

  #[test]
  #[cfg(unix)]
  fn runtime_dirs_are_created_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let paths = Paths::new(
      &home(),
      Some(temp.path().join("config").as_os_str()),
      Some(temp.path().join("data").as_os_str()),
    );

    paths.create_runtime_dirs().unwrap();

    for dir in [paths.config_dir(), &paths.data_root, &paths.store] {
      let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
      assert_eq!(mode, 0o700, "expected 0700 on {}", dir.display());
    }
  }
}
