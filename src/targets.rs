#![allow(dead_code)] // consumed by the command tickets

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::paths::{canonicalize_existing_prefix, resolve_user_path};
use crate::skill;
use crate::source::{Source, parse_source};
use crate::transaction::{ExpectedLink, Transaction};

/// One configured target, resolved for filesystem work and canonicalized for
/// conflict detection (symlinked parents collapse to one canonical location).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTarget {
  pub skill: String,
  pub configured: PathBuf,
  pub resolved: PathBuf,
  canonical: PathBuf,
}

/// Validates the complete target graph before any command mutates anything.
/// `protected` is spm's own territory: the config directory and data root.
pub fn plan_targets(
  config: &Config,
  home: &Path,
  protected: &[PathBuf],
) -> Result<Vec<ResolvedTarget>> {
  let mut targets: Vec<ResolvedTarget> = Vec::new();

  for (name, skill) in &config.skills {
    for configured in &skill.targets {
      let resolved = resolve_user_path(configured, home)
        .with_context(|| format!("invalid target for skill '{name}'"))?;
      skill::ensure_target_matches(&resolved, name)?;
      let canonical = canonical_target(&resolved)?;

      targets.push(ResolvedTarget {
        skill: name.clone(),
        configured: configured.clone(),
        resolved,
        canonical,
      });
    }
  }

  // pairwise: normalized duplicates, cross-skill sharing, nesting
  for (index, a) in targets.iter().enumerate() {
    for b in &targets[index + 1..] {
      if a.canonical == b.canonical {
        if a.skill == b.skill {
          bail!(
            "skill '{}' lists target '{}' twice (as '{}' and '{}')",
            a.skill,
            a.canonical.display(),
            a.configured.display(),
            b.configured.display()
          );
        }
        bail!(
          "target '{}' is shared by skills '{}' and '{}'",
          a.canonical.display(),
          a.skill,
          b.skill
        );
      }
      if a.canonical.starts_with(&b.canonical) || b.canonical.starts_with(&a.canonical) {
        bail!(
          "target '{}' (skill '{}') is nested inside target '{}' (skill '{}')",
          descendant(a, b).canonical.display(),
          descendant(a, b).skill,
          ancestor(a, b).canonical.display(),
          ancestor(a, b).skill
        );
      }
    }
  }

  // spm's own directories
  for guarded in protected {
    let guarded = canonicalize_existing_prefix(guarded)?;
    for target in &targets {
      if overlaps(&target.canonical, &guarded) {
        bail!(
          "target '{}' (skill '{}') overlaps spm's own directory '{}'",
          target.canonical.display(),
          target.skill,
          guarded.display()
        );
      }
    }
  }

  // configured local sources
  for (name, skill) in &config.skills {
    let Source::Local(local) = parse_source(&skill.source)? else {
      continue;
    };
    let source_dir = canonicalize_existing_prefix(&resolve_user_path(&local.path, home)?)?;
    for target in &targets {
      if overlaps(&target.canonical, &source_dir) {
        bail!(
          "target '{}' (skill '{}') overlaps the local source '{}' of skill '{name}'",
          target.canonical.display(),
          target.skill,
          source_dir.display()
        );
      }
    }
  }

  Ok(targets)
}

/// Canonicalizes only the parent chain and reattaches the basename: an
/// already-installed target is itself a symlink into the store, and following
/// it would misreport every reinstall as a protected-directory overlap (and
/// collapse distinct targets of one snapshot into "duplicates").
/// This is the one canonical identity for targets; merging uses it too.
pub(crate) fn canonical_target(resolved: &Path) -> Result<PathBuf> {
  let name = resolved
    .file_name()
    .with_context(|| format!("target '{}' has no final component", resolved.display()))?;
  let parent = resolved
    .parent()
    .with_context(|| format!("target '{}' has no parent", resolved.display()))?;
  Ok(canonicalize_existing_prefix(parent)?.join(name))
}

fn overlaps(a: &Path, b: &Path) -> bool {
  a.starts_with(b) || b.starts_with(a)
}

fn ancestor<'a>(a: &'a ResolvedTarget, b: &'a ResolvedTarget) -> &'a ResolvedTarget {
  if b.canonical.starts_with(&a.canonical) {
    a
  } else {
    b
  }
}

fn descendant<'a>(a: &'a ResolvedTarget, b: &'a ResolvedTarget) -> &'a ResolvedTarget {
  if b.canonical.starts_with(&a.canonical) {
    b
  } else {
    a
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InstallAction {
  Create,
  Replace,
  /// The correct link already exists; nothing is staged or rewritten.
  Noop,
}

/// Preflights one target and stages the work to make it an absolute symlink
/// to `destination`. Regular files and directories always fail — there is no
/// force path. The transaction re-checks the observed state at commit.
pub fn stage_install(
  transaction: &mut Transaction,
  target: &ResolvedTarget,
  destination: &Path,
) -> Result<InstallAction> {
  if !destination.is_absolute() {
    bail!(
      "refusing to link '{}' to relative destination '{}'; targets are always absolute symlinks",
      target.resolved.display(),
      destination.display()
    );
  }

  match entry_state(&target.resolved)? {
    EntryState::Absent => {
      if let Some(parent) = target.resolved.parent() {
        transaction.create_dirs(parent);
      }
      transaction.set_symlink(&target.resolved, destination, ExpectedLink::Absent);
      Ok(InstallAction::Create)
    }
    EntryState::Symlink(current) if current == destination => Ok(InstallAction::Noop),
    // wrong or dangling link: replaced atomically, destination untouched
    EntryState::Symlink(_) => {
      transaction.set_symlink(&target.resolved, destination, ExpectedLink::AnySymlink);
      Ok(InstallAction::Replace)
    }
    EntryState::Other => bail!(
      "target '{}' (skill '{}') is a regular file or directory; refusing to replace it",
      target.resolved.display(),
      target.skill
    ),
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemovalAction {
  /// A missing target is already removed.
  AlreadyMissing,
  Unlink,
}

/// Preflights one target for removal. Only symlinks are ever unlinked; a
/// regular file or directory aborts the whole removal.
pub fn stage_removal(transaction: &mut Transaction, target: &Path) -> Result<RemovalAction> {
  match entry_state(target)? {
    EntryState::Absent => Ok(RemovalAction::AlreadyMissing),
    EntryState::Symlink(_) => {
      transaction.remove_symlink(target);
      Ok(RemovalAction::Unlink)
    }
    EntryState::Other => bail!(
      "target '{}' is a regular file or directory; refusing to remove it",
      target.display()
    ),
  }
}

enum EntryState {
  Absent,
  Symlink(PathBuf),
  Other,
}

fn entry_state(path: &Path) -> Result<EntryState> {
  match fs::symlink_metadata(path) {
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(EntryState::Absent),
    Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    Ok(metadata) if metadata.file_type().is_symlink() => {
      let destination =
        fs::read_link(path).with_context(|| format!("failed to read {}", path.display()))?;
      Ok(EntryState::Symlink(destination))
    }
    Ok(_) => Ok(EntryState::Other),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::Skill;
  use std::collections::BTreeMap;

  struct World {
    temp: tempfile::TempDir,
    home: PathBuf,
  }

  fn world() -> World {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    World { temp, home }
  }

  impl World {
    fn protected(&self) -> Vec<PathBuf> {
      vec![
        self.home.join(".config/spm"),
        self.home.join(".local/share/spm"),
      ]
    }

    fn config(&self, skills: &[(&str, &str, &[&str])]) -> Config {
      let mut map = BTreeMap::new();
      for (name, source, targets) in skills {
        map.insert(
          name.to_string(),
          Skill {
            source: source.to_string(),
            r#ref: None,
            targets: targets.iter().map(PathBuf::from).collect(),
          },
        );
      }
      Config {
        version: 1,
        skills: map,
      }
    }

    fn plan(&self, skills: &[(&str, &str, &[&str])]) -> Result<Vec<ResolvedTarget>> {
      plan_targets(&self.config(skills), &self.home, &self.protected())
    }
  }

  #[test]
  fn valid_targets_resolve_from_home_and_tilde() {
    let world = world();
    let targets = world
      .plan(&[(
        "my-skill",
        "github:o/r/my-skill",
        &[".claude/skills/my-skill", "~/.agents/skills/my-skill"],
      )])
      .unwrap();

    assert_eq!(targets.len(), 2);
    assert_eq!(
      targets[0].resolved,
      world.home.join(".claude/skills/my-skill")
    );
    assert_eq!(
      targets[1].resolved,
      world.home.join(".agents/skills/my-skill")
    );
  }

  #[test]
  fn basename_must_equal_the_skill_name() {
    let world = world();
    let error = world
      .plan(&[(
        "my-skill",
        "github:o/r/my-skill",
        &[".claude/skills/renamed"],
      )])
      .unwrap_err();
    assert!(error.to_string().contains("must end in the skill name"));
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_parent_aliases_are_normalized_duplicates() {
    let world = world();
    fs::create_dir_all(world.home.join("real")).unwrap();
    std::os::unix::fs::symlink(world.home.join("real"), world.home.join("alias")).unwrap();

    let error = world
      .plan(&[(
        "my-skill",
        "github:o/r/my-skill",
        &["real/my-skill", "alias/my-skill"],
      )])
      .unwrap_err();
    assert!(
      error.to_string().contains("twice"),
      "aliased spellings must collapse: {error}"
    );
  }

  #[test]
  fn nested_targets_are_rejected() {
    let world = world();
    let error = world
      .plan(&[
        ("outer", "github:o/r/outer", &["skills/outer"]),
        ("inner", "github:o/r/inner", &["skills/outer/inner"]),
      ])
      .unwrap_err();
    assert!(error.to_string().contains("nested inside"));
  }

  #[test]
  fn spm_directories_are_protected() {
    let world = world();

    // inside the data root
    let error = world
      .plan(&[("x", "github:o/r/x", &[".local/share/spm/store/x"])])
      .unwrap_err();
    assert!(error.to_string().contains("spm's own directory"));

    // the config dir itself (a skill named "spm" targeting it)
    let error = world
      .plan(&[("spm", "github:o/r/spm", &[".config/spm"])])
      .unwrap_err();
    assert!(error.to_string().contains("spm's own directory"));

    // an ancestor of the data root
    let error = world
      .plan(&[("share", "github:o/r/share", &[".local/share"])])
      .unwrap_err();
    assert!(error.to_string().contains("spm's own directory"));
  }

  #[test]
  fn local_source_overlap_is_rejected() {
    let world = world();
    fs::create_dir_all(world.home.join("skills/local-skill")).unwrap();

    // a target inside another skill's local source directory
    let error = world
      .plan(&[
        ("local-skill", "skills/local-skill", &["links/local-skill"]),
        (
          "nested",
          "github:o/r/nested",
          &["skills/local-skill/nested"],
        ),
      ])
      .unwrap_err();
    assert!(error.to_string().contains("overlaps the local source"));

    // a skill whose own target IS its source
    let error = world
      .plan(&[("local-skill", "skills/local-skill", &["skills/local-skill"])])
      .unwrap_err();
    assert!(error.to_string().contains("overlaps the local source"));
  }

  fn single_target(world: &World, name: &str) -> ResolvedTarget {
    world
      .plan(&[(name, "github:o/r/x", &[&format!("links/deep/{name}")])])
      .unwrap()
      .remove(0)
  }

  #[test]
  #[cfg(unix)]
  fn install_creates_noops_and_replaces() {
    let world = world();
    let destination = world.temp.path().join("store/sha256/aaaa");
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("precious"), "data").unwrap();

    let target = single_target(&world, "x");

    // missing target: created together with its parents
    let mut tx = Transaction::new();
    assert_eq!(
      stage_install(&mut tx, &target, &destination).unwrap(),
      InstallAction::Create
    );
    tx.commit().unwrap();
    assert_eq!(fs::read_link(&target.resolved).unwrap(), destination);

    // correct link: nothing staged
    let mut tx = Transaction::new();
    assert_eq!(
      stage_install(&mut tx, &target, &destination).unwrap(),
      InstallAction::Noop
    );
    tx.commit().unwrap();

    // wrong link: replaced atomically without touching the old destination
    let other = world.temp.path().join("store/sha256/bbbb");
    fs::create_dir_all(&other).unwrap();
    let mut tx = Transaction::new();
    assert_eq!(
      stage_install(&mut tx, &target, &other).unwrap(),
      InstallAction::Replace
    );
    tx.commit().unwrap();
    assert_eq!(fs::read_link(&target.resolved).unwrap(), other);
    assert_eq!(
      fs::read_to_string(destination.join("precious")).unwrap(),
      "data",
      "the previous destination must never be touched"
    );

    // dangling link: also replaced
    fs::remove_file(&target.resolved).unwrap();
    std::os::unix::fs::symlink("/nowhere/at/all", &target.resolved).unwrap();
    let mut tx = Transaction::new();
    assert_eq!(
      stage_install(&mut tx, &target, &destination).unwrap(),
      InstallAction::Replace
    );
    tx.commit().unwrap();
    assert_eq!(fs::read_link(&target.resolved).unwrap(), destination);
  }

  #[test]
  #[cfg(unix)]
  fn already_installed_targets_still_plan_cleanly() {
    let world = world();

    // simulate a prior install: both targets are symlinks into the store
    let snapshot = world
      .home
      .join(".local/share/spm/store/sha256")
      .join("a".repeat(64));
    fs::create_dir_all(&snapshot).unwrap();
    for parent in ["one", "two"] {
      let dir = world.home.join(parent);
      fs::create_dir_all(&dir).unwrap();
      std::os::unix::fs::symlink(&snapshot, dir.join("my-skill")).unwrap();
    }

    // planning again must neither report a protected-directory overlap (the
    // links resolve into the store) nor collapse the two targets into one
    let targets = world
      .plan(&[(
        "my-skill",
        "github:o/r/my-skill",
        &["one/my-skill", "two/my-skill"],
      )])
      .unwrap();
    assert_eq!(targets.len(), 2);
  }

  #[test]
  fn relative_destinations_are_rejected() {
    let world = world();
    let target = single_target(&world, "x");

    let mut tx = Transaction::new();
    let error = stage_install(&mut tx, &target, Path::new("store/relative")).unwrap_err();
    assert!(error.to_string().contains("always absolute"));
  }

  #[test]
  fn regular_files_and_directories_are_never_replaced() {
    let world = world();
    let destination = world.temp.path().join("dest");
    fs::create_dir_all(&destination).unwrap();

    let target = single_target(&world, "x");
    fs::create_dir_all(target.resolved.parent().unwrap()).unwrap();

    for build in [
      (|path: &Path| fs::write(path, "user file").unwrap()) as fn(&Path),
      |path| fs::create_dir(path).unwrap(),
    ] {
      let _ = fs::remove_file(&target.resolved);
      let _ = fs::remove_dir(&target.resolved);
      build(&target.resolved);

      let mut tx = Transaction::new();
      let error = stage_install(&mut tx, &target, &destination).unwrap_err();
      assert!(error.to_string().contains("refusing to replace"));
      assert!(target.resolved.exists(), "the user's data must survive");
    }
  }

  #[test]
  #[cfg(unix)]
  fn removal_accepts_missing_unlinks_symlinks_and_aborts_on_files() {
    let world = world();
    let links = world.home.join("links");
    fs::create_dir(&links).unwrap();

    let missing = links.join("missing");
    let linked = links.join("linked");
    std::os::unix::fs::symlink("/somewhere", &linked).unwrap();
    let file = links.join("file");
    fs::write(&file, "user data").unwrap();

    let mut tx = Transaction::new();
    assert_eq!(
      stage_removal(&mut tx, &missing).unwrap(),
      RemovalAction::AlreadyMissing
    );
    assert_eq!(
      stage_removal(&mut tx, &linked).unwrap(),
      RemovalAction::Unlink
    );

    // the file target aborts the whole plan before anything commits
    let error = stage_removal(&mut tx, &file).unwrap_err();
    assert!(error.to_string().contains("refusing to remove"));

    // nothing was committed: the symlink is still there
    assert!(
      fs::symlink_metadata(&linked)
        .unwrap()
        .file_type()
        .is_symlink()
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "user data");
  }

  #[test]
  #[cfg(unix)]
  fn multi_target_failures_roll_back_earlier_link_changes() {
    let world = world();
    let destination = world.temp.path().join("dest");
    fs::create_dir_all(&destination).unwrap();

    let targets = world
      .plan(&[("x", "github:o/r/x", &["links/a/x", "links/b/x"])])
      .unwrap();

    let mut tx = Transaction::new();
    for target in &targets {
      stage_install(&mut tx, target, &destination).unwrap();
    }

    // sabotage the second target between staging and commit: a file appears
    fs::create_dir_all(targets[1].resolved.parent().unwrap()).unwrap();
    fs::write(&targets[1].resolved, "squatter").unwrap();

    let error = tx.commit().unwrap_err();
    assert!(error.to_string().contains("changed since it was planned"));

    // the first link (and its created parents) were rolled back
    assert!(!targets[0].resolved.exists());
    assert!(!world.home.join("links/a").exists());
    assert_eq!(
      fs::read_to_string(&targets[1].resolved).unwrap(),
      "squatter"
    );
  }
}
