#![allow(dead_code)] // consumed by the command tickets

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::paths::canonicalize_existing_prefix;

pub const CONFIG_VERSION: i64 = 1;

/// What `add` writes for a brand-new config and what remains after the last
/// removal of a user-authored one may differ in comments; this is only the
/// bootstrap form.
const EMPTY_CONFIG: &str = "version = 1\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
  pub version: i64,
  pub skills: BTreeMap<String, Skill>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
  pub source: String,
  pub r#ref: Option<String>,
  /// Stored exactly as authored; resolution happens in the paths layer.
  pub targets: Vec<PathBuf>,
}

/// A loaded `spm.toml` that supports surgical edits: comments, ordering, and
/// unrelated formatting survive add/remove.
#[derive(Debug)]
pub struct ConfigDocument {
  doc: DocumentMut,
  /// The logical global path, possibly a symlink.
  path: PathBuf,
  /// Where the bytes actually live; writes go here so the symlink survives.
  real_path: PathBuf,
  /// Bytes as loaded; None means the file did not exist yet.
  original: Option<Vec<u8>>,
}

impl ConfigDocument {
  pub fn load(path: &Path) -> Result<Self> {
    let bytes =
      fs::read(path).with_context(|| format!("failed to read config {}", path.display()))?;
    Self::from_bytes(path, bytes)
  }

  /// For `add`, the only command allowed to bootstrap an absent config.
  pub fn load_or_empty(path: &Path) -> Result<Self> {
    match fs::read(path) {
      Ok(bytes) => Self::from_bytes(path, bytes),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self {
        doc: EMPTY_CONFIG.parse().expect("canonical empty config parses"),
        // resolve dangling symlinks too, so save() creates the link's target
        // instead of replacing the link with a regular file
        real_path: resolve_real_path(path)?,
        path: path.to_path_buf(),
        original: None,
      }),
      Err(error) => Err(error).with_context(|| format!("failed to read config {}", path.display())),
    }
  }

  fn from_bytes(path: &Path, bytes: Vec<u8>) -> Result<Self> {
    let text =
      str::from_utf8(&bytes).with_context(|| format!("config {} is not UTF-8", path.display()))?;
    let doc: DocumentMut = text
      .parse()
      .with_context(|| format!("config {} is not valid TOML", path.display()))?;
    validate(&doc)?;

    Ok(Self {
      doc,
      real_path: canonicalize_existing_prefix(path)?,
      path: path.to_path_buf(),
      original: Some(bytes),
    })
  }

  /// The validated model of the document's current (possibly edited) state.
  pub fn config(&self) -> Result<Config> {
    validate(&self.doc)
  }

  /// Inserts or updates one skill entry, leaving everything else untouched.
  pub fn upsert_skill(&mut self, name: &str, skill: &Skill) -> Result<()> {
    self.write_skill_entry(name, skill)?;
    // the schema stays strict through edits, not just loads
    validate(&self.doc)?;
    Ok(())
  }

  fn write_skill_entry(&mut self, name: &str, skill: &Skill) -> Result<()> {
    let targets = targets_array(&skill.targets)?;

    let skills = self.doc.entry("skills").or_insert_with(|| {
      let mut table = Table::new();
      table.set_implicit(true); // don't emit a bare [skills] header
      Item::Table(table)
    });
    let Some(skills) = skills.as_table_like_mut() else {
      bail!("'skills' is not a table");
    };

    if let Some(existing) = skills.get_mut(name).and_then(Item::as_table_like_mut) {
      existing.insert("source", value(skill.source.as_str()));
      match &skill.r#ref {
        Some(r) => {
          existing.insert("ref", value(r.as_str()));
        }
        None => {
          existing.remove("ref");
        }
      }
      existing.insert("targets", targets);
      return Ok(());
    }

    let mut table = Table::new();
    table.insert("source", value(skill.source.as_str()));
    if let Some(r) = &skill.r#ref {
      table.insert("ref", value(r.as_str()));
    }
    table.insert("targets", targets);
    skills.insert(name, Item::Table(table));
    Ok(())
  }

  pub fn remove_skill(&mut self, name: &str) -> Result<()> {
    let removed = self
      .doc
      .get_mut("skills")
      .and_then(Item::as_table_like_mut)
      .and_then(|skills| skills.remove(name));

    if removed.is_none() {
      bail!("skill '{name}' is not in spm.toml");
    }
    Ok(())
  }

  /// Atomic write via a temporary sibling of the real file; a symlinked
  /// config path stays a symlink.
  pub fn save(&self) -> Result<()> {
    validate(&self.doc).context("refusing to save an invalid config")?;

    let parent = self.real_path.parent().with_context(|| {
      format!(
        "config {} has no parent directory",
        self.real_path.display()
      )
    })?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
      .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(self.doc.to_string().as_bytes())?;

    // keep the user's permissions when overwriting
    if let Ok(metadata) = fs::metadata(&self.real_path) {
      temp.as_file().set_permissions(metadata.permissions())?;
    }

    temp.as_file().sync_all()?;
    temp
      .persist(&self.real_path)
      .with_context(|| format!("failed to write config {}", self.real_path.display()))?;
    Ok(())
  }

  /// Pre-commit check: have the on-disk bytes changed since load?
  pub fn externally_modified(&self) -> Result<bool> {
    match fs::read(&self.path) {
      Ok(bytes) => Ok(self.original.as_deref() != Some(bytes.as_slice())),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(self.original.is_some()),
      Err(error) => {
        Err(error).with_context(|| format!("failed to read config {}", self.path.display()))
      }
    }
  }
}

/// Follows a symlink chain even when the final target does not exist yet,
/// which plain canonicalize reports as NotFound.
fn resolve_real_path(path: &Path) -> Result<PathBuf> {
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

fn targets_array(targets: &[PathBuf]) -> Result<Item> {
  let mut array = Array::new();
  for target in targets {
    let text = target
      .to_str()
      .with_context(|| format!("target {} is not UTF-8", target.display()))?;
    array.push(text);
  }
  Ok(value(array))
}

fn validate(doc: &DocumentMut) -> Result<Config> {
  for (key, _) in doc.iter() {
    if key != "version" && key != "skills" {
      bail!("unknown top-level field '{key}' in spm.toml");
    }
  }

  let Some(version) = doc.get("version") else {
    bail!("spm.toml is missing 'version'");
  };
  let Some(version) = version.as_integer() else {
    bail!("'version' must be an integer");
  };
  if version != CONFIG_VERSION {
    bail!("unsupported config version {version}; this spm supports version {CONFIG_VERSION}");
  }

  let mut skills = BTreeMap::new();

  if let Some(item) = doc.get("skills") {
    let Some(table) = item.as_table_like() else {
      bail!("'skills' must be a table");
    };

    for (name, item) in table.iter() {
      let skill = validate_skill(name, item)
        .with_context(|| format!("invalid skill '{name}' in spm.toml"))?;
      skills.insert(name.to_string(), skill);
    }
  }

  Ok(Config { version, skills })
}

fn validate_skill(name: &str, item: &Item) -> Result<Skill> {
  let Some(table) = item.as_table_like() else {
    bail!("skill '{name}' must be a table");
  };

  for (key, _) in table.iter() {
    if !matches!(key, "source" | "ref" | "targets") {
      bail!("unknown field '{key}'");
    }
  }

  let Some(source) = table.get("source") else {
    bail!("missing 'source'");
  };
  let Some(source) = source.as_str() else {
    bail!("'source' must be a string");
  };
  if source.is_empty() {
    bail!("'source' must not be empty");
  }

  let r#ref = match table.get("ref") {
    None => None,
    Some(item) => {
      let Some(r) = item.as_str() else {
        bail!("'ref' must be a string");
      };
      if r.is_empty() {
        bail!("'ref' must not be empty");
      }
      Some(r.to_string())
    }
  };

  let Some(targets) = table.get("targets") else {
    bail!("missing 'targets'");
  };
  let Some(targets) = targets.as_array() else {
    bail!("'targets' must be an array");
  };
  if targets.is_empty() {
    bail!("'targets' must not be empty");
  }

  let mut parsed: Vec<PathBuf> = Vec::new();
  for entry in targets {
    let Some(text) = entry.as_str() else {
      bail!("'targets' entries must be strings");
    };
    if text.is_empty() {
      bail!("'targets' entries must not be empty");
    }
    if parsed.iter().any(|existing| existing.as_os_str() == text) {
      bail!("duplicate target '{text}'");
    }
    parsed.push(PathBuf::from(text));
  }

  Ok(Skill {
    source: source.to_string(),
    r#ref,
    targets: parsed,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  const SAMPLE: &str = r#"# my skills
version = 1

# design lives on main
[skills.frontend-design]
source = "github:anthropics/skills/frontend-design"
ref = "main"
targets = [
  ".claude/skills/frontend-design", # primary
  ".agents/skills/frontend-design",
]

[skills.my-local-skill]
source = "skills/my-local-skill"
targets = [".claude/skills/my-local-skill"]
"#;

  fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("spm.toml");
    fs::write(&path, contents).unwrap();
    path
  }

  fn load(contents: &str) -> Result<Config> {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), contents);
    ConfigDocument::load(&path).map(|doc| doc.config().unwrap())
  }

  #[test]
  fn valid_config_parses_with_authored_strings() {
    let config = load(SAMPLE).unwrap();

    assert_eq!(config.version, 1);
    assert_eq!(config.skills.len(), 2);

    let design = &config.skills["frontend-design"];
    assert_eq!(design.source, "github:anthropics/skills/frontend-design");
    assert_eq!(design.r#ref.as_deref(), Some("main"));
    assert_eq!(
      design.targets,
      vec![
        PathBuf::from(".claude/skills/frontend-design"),
        PathBuf::from(".agents/skills/frontend-design"),
      ]
    );

    // relative source stays exactly as authored
    let local = &config.skills["my-local-skill"];
    assert_eq!(local.source, "skills/my-local-skill");
    assert_eq!(local.r#ref, None);
  }

  #[test]
  fn skills_table_is_optional() {
    let config = load("version = 1\n").unwrap();
    assert!(config.skills.is_empty());
  }

  #[test]
  fn invalid_forms_are_rejected() {
    let cases: &[(&str, &str)] = &[
      ("version = 1\nextra = true\n", "unknown top-level field"),
      ("", "missing 'version'"),
      ("version = \"1\"\n", "must be an integer"),
      ("version = 2\n", "unsupported config version 2"),
      ("version = 0\n", "unsupported config version 0"),
      ("version = 1\nskills = 3\n", "'skills' must be a table"),
      ("version = 1\n[skills]\nx = 1\n", "must be a table"),
      (
        "version = 1\n[skills.x]\ntargets = [\"t/x\"]\n",
        "missing 'source'",
      ),
      (
        "version = 1\n[skills.x]\nsource = 1\ntargets = [\"t/x\"]\n",
        "'source' must be a string",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"\"\ntargets = [\"t/x\"]\n",
        "'source' must not be empty",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\nref = 1\ntargets = [\"t/x\"]\n",
        "'ref' must be a string",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\nref = \"\"\ntargets = [\"t/x\"]\n",
        "'ref' must not be empty",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\n",
        "missing 'targets'",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = \"t/x\"\n",
        "'targets' must be an array",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = []\n",
        "'targets' must not be empty",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = [1]\n",
        "'targets' entries must be strings",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = [\"\"]\n",
        "must not be empty",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = [\"t/x\", \"t/x\"]\n",
        "duplicate target 't/x'",
      ),
      (
        "version = 1\n[skills.x]\nsource = \"s/x\"\ntargets = [\"t/x\"]\nalias = \"y\"\n",
        "unknown field 'alias'",
      ),
      ("version = 1\n[skills.x\n", "not valid TOML"),
    ];

    for (contents, expected) in cases {
      let error = load(contents).unwrap_err();
      let message = format!("{error:#}");
      assert!(
        message.contains(expected),
        "expected '{expected}' for {contents:?}, got: {message}"
      );
    }
  }

  #[test]
  fn add_preserves_comments_and_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc
      .upsert_skill(
        "new-skill",
        &Skill {
          source: "skills/new-skill".into(),
          r#ref: None,
          targets: vec![PathBuf::from(".claude/skills/new-skill")],
        },
      )
      .unwrap();
    doc.save().unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert!(
      written.starts_with(SAMPLE),
      "existing text must survive byte-for-byte:\n{written}"
    );
    assert!(written.contains("[skills.new-skill]"));

    let config = ConfigDocument::load(&path).unwrap().config().unwrap();
    assert_eq!(config.skills.len(), 3);
  }

  #[test]
  fn remove_preserves_unrelated_entries_and_comments() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc.remove_skill("my-local-skill").unwrap();
    doc.save().unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("# my skills"));
    assert!(written.contains("# design lives on main"));
    assert!(written.contains("# primary"));
    assert!(!written.contains("my-local-skill"));
  }

  #[test]
  fn upsert_updates_an_existing_entry_in_place() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc
      .upsert_skill(
        "frontend-design",
        &Skill {
          source: "github:anthropics/skills/frontend-design".into(),
          r#ref: Some("v2".into()),
          targets: vec![PathBuf::from(".claude/skills/frontend-design")],
        },
      )
      .unwrap();
    doc.save().unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert!(
      written.contains("# design lives on main"),
      "entry comment survives"
    );
    assert!(written.contains("ref = \"v2\""));

    let config = ConfigDocument::load(&path).unwrap().config().unwrap();
    assert_eq!(
      config.skills["frontend-design"].r#ref.as_deref(),
      Some("v2")
    );
    assert_eq!(config.skills["frontend-design"].targets.len(), 1);
  }

  #[test]
  fn upsert_can_drop_a_ref() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc
      .upsert_skill(
        "frontend-design",
        &Skill {
          source: "github:anthropics/skills/frontend-design".into(),
          r#ref: None,
          targets: vec![PathBuf::from(".claude/skills/frontend-design")],
        },
      )
      .unwrap();

    assert_eq!(doc.config().unwrap().skills["frontend-design"].r#ref, None);
  }

  #[test]
  fn removing_the_last_skill_leaves_a_valid_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(
      temp.path(),
      "version = 1\n\n[skills.x]\nsource = \"s/x\"\ntargets = [\"t/x\"]\n",
    );

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc.remove_skill("x").unwrap();
    doc.save().unwrap();

    let config = ConfigDocument::load(&path).unwrap().config().unwrap();
    assert!(config.skills.is_empty());
  }

  #[test]
  fn removing_an_unknown_skill_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    let error = doc.remove_skill("nope").unwrap_err();
    assert!(error.to_string().contains("'nope'"));
  }

  #[test]
  fn load_or_empty_bootstraps_a_canonical_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("spm.toml");

    let doc = ConfigDocument::load_or_empty(&path).unwrap();
    let config = doc.config().unwrap();
    assert_eq!(config.version, 1);
    assert!(config.skills.is_empty());

    doc.save().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "version = 1\n");
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_config_is_edited_through_the_link() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-spm.toml");
    fs::write(&real, SAMPLE).unwrap();

    let link = temp.path().join("spm.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let mut doc = ConfigDocument::load(&link).unwrap();
    doc.remove_skill("my-local-skill").unwrap();
    doc.save().unwrap();

    // the symlink survives and still points at the same place
    let meta = fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), real);

    let written = fs::read_to_string(&real).unwrap();
    assert!(!written.contains("my-local-skill"));
  }

  #[test]
  #[cfg(unix)]
  fn dangling_config_symlink_is_not_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-spm.toml");
    let link = temp.path().join("spm.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let doc = ConfigDocument::load_or_empty(&link).unwrap();
    doc.save().unwrap();

    let meta = fs::symlink_metadata(&link).unwrap();
    assert!(
      meta.file_type().is_symlink(),
      "the dangling link must survive"
    );
    assert_eq!(fs::read_link(&link).unwrap(), real);
    assert_eq!(fs::read_to_string(&real).unwrap(), "version = 1\n");
  }

  #[test]
  fn upsert_rejects_invalid_skill_input() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);
    let mut doc = ConfigDocument::load(&path).unwrap();

    let base = Skill {
      source: "skills/x".into(),
      r#ref: None,
      targets: vec![PathBuf::from("t/x")],
    };

    let empty_source = Skill {
      source: String::new(),
      ..base.clone()
    };
    let error = doc.upsert_skill("x", &empty_source).unwrap_err();
    assert!(format!("{error:#}").contains("'source' must not be empty"));

    let empty_ref = Skill {
      r#ref: Some(String::new()),
      ..base.clone()
    };
    assert!(doc.upsert_skill("x", &empty_ref).is_err());

    let no_targets = Skill {
      targets: vec![],
      ..base.clone()
    };
    assert!(doc.upsert_skill("x", &no_targets).is_err());

    let duplicate_targets = Skill {
      targets: vec![PathBuf::from("t/x"), PathBuf::from("t/x")],
      ..base
    };
    assert!(doc.upsert_skill("x", &duplicate_targets).is_err());
  }

  #[test]
  fn save_refuses_an_invalid_document() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);
    let mut doc = ConfigDocument::load(&path).unwrap();

    // a failed upsert leaves the in-memory doc dirty; save must not persist it
    let _ = doc.upsert_skill(
      "x",
      &Skill {
        source: String::new(),
        r#ref: None,
        targets: vec![PathBuf::from("t/x")],
      },
    );

    let error = doc.save().unwrap_err();
    assert!(format!("{error:#}").contains("refusing to save"));
    assert_eq!(
      fs::read_to_string(&path).unwrap(),
      SAMPLE,
      "disk must be untouched"
    );
  }

  #[test]
  fn save_leaves_no_temporary_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let mut doc = ConfigDocument::load(&path).unwrap();
    doc.remove_skill("my-local-skill").unwrap();
    doc.save().unwrap();

    let entries: Vec<_> = fs::read_dir(temp.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("spm.toml")]);
  }

  #[test]
  fn external_modification_is_detected() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(temp.path(), SAMPLE);

    let doc = ConfigDocument::load(&path).unwrap();
    assert!(!doc.externally_modified().unwrap());

    fs::write(&path, "version = 1\n").unwrap();
    assert!(doc.externally_modified().unwrap());

    fs::remove_file(&path).unwrap();
    assert!(doc.externally_modified().unwrap());
  }
}
