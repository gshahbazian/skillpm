use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::Config;

pub const LOCK_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Lockfile {
  pub version: i64,
  pub skills: BTreeMap<String, LockedSkill>,
}

impl Lockfile {
  pub fn empty() -> Self {
    Self {
      version: LOCK_VERSION,
      skills: BTreeMap::new(),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LockedSkill {
  /// Mirrors skillpm.toml for stale-lock detection.
  pub source: String,
  pub r#ref: Option<String>,
  /// Exact resolved commit; present for GitHub sources only.
  pub commit: Option<String>,
  /// "sha256:" + 64 hex digits of the snapshot content hash.
  pub content_hash: String,
}

/// Everything install/add/remove must distinguish before trusting the lock.
/// Only `update` may regenerate the non-Valid, non-Newer states.
#[derive(Debug)]
pub enum LockState {
  Missing,
  Malformed { reason: String },
  Older { version: i64 },
  Newer { version: i64 },
  Valid { lockfile: Lockfile },
}

/// A valid, config-matching lockfile plus its original bytes for the
/// pre-commit external-change check.
#[derive(Debug)]
pub struct LockfileDocument {
  pub lockfile: Lockfile,
  path: PathBuf,
  original: Option<Vec<u8>>,
}

#[cfg_attr(not(test), allow(dead_code))] // commands use the two gates below
pub fn read_lock_state(path: &Path) -> Result<LockState> {
  match read_optional(path)? {
    Some(bytes) => Ok(classify(&bytes)),
    None => Ok(LockState::Missing),
  }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
  match fs::read(path) {
    Ok(bytes) => Ok(Some(bytes)),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error).with_context(|| format!("failed to read lockfile {}", path.display())),
  }
}

fn classify(bytes: &[u8]) -> LockState {
  let malformed = |reason: String| LockState::Malformed { reason };

  let Ok(text) = str::from_utf8(bytes) else {
    return malformed("not UTF-8".into());
  };
  let doc: DocumentMut = match text.parse() {
    Ok(doc) => doc,
    Err(error) => return malformed(format!("invalid TOML: {error}")),
  };

  // version decides before strict schema checks: a newer lockfile may have
  // fields this skillpm has never heard of, and that is not "malformed"
  let Some(version) = doc.get("version").and_then(Item::as_integer) else {
    return malformed("missing or non-integer 'version'".into());
  };
  if version > LOCK_VERSION {
    return LockState::Newer { version };
  }
  if version < LOCK_VERSION {
    return LockState::Older { version };
  }

  match parse_v1(&doc) {
    Ok(lockfile) => LockState::Valid { lockfile },
    Err(error) => malformed(format!("{error:#}")),
  }
}

/// The strict gate for install, add, and remove: exact entry set with
/// matching source/ref data. Target-only config edits do not go stale.
pub fn require_fresh(path: &Path, config: &Config) -> Result<LockfileDocument> {
  let bytes = read_optional(path)?;
  let state = match &bytes {
    Some(bytes) => classify(bytes),
    None => LockState::Missing,
  };

  match state {
    LockState::Missing => bail!("skillpm.lock is missing; run `skillpm update`"),
    LockState::Malformed { reason } => {
      bail!("skillpm.lock is malformed ({reason}); run `skillpm update`")
    }
    LockState::Older { version } => bail!(
      "skillpm.lock version {version} is older than supported version {LOCK_VERSION}; run `skillpm update`"
    ),
    LockState::Newer { version } => {
      bail!("skillpm.lock version {version} is newer than this skillpm supports; upgrade skillpm")
    }
    LockState::Valid { lockfile } => {
      check_matches_config(&lockfile, config)?;
      Ok(LockfileDocument {
        lockfile,
        path: path.to_path_buf(),
        original: bytes,
      })
    }
  }
}

/// What `update` starts from: reusable entries when the old lock was valid,
/// plus the original bytes (even of malformed/older state) so the pre-commit
/// external-change check still works.
#[derive(Debug)]
pub struct UpdateLockDocument {
  /// Entries for snapshot reuse; None when the lock must regenerate from scratch.
  pub reusable: Option<Lockfile>,
  path: PathBuf,
  original: Option<Vec<u8>>,
}

impl UpdateLockDocument {
  /// Pre-commit check: have the on-disk bytes changed since load?
  pub fn externally_modified(&self) -> Result<bool> {
    on_disk_bytes_changed(&self.path, self.original.as_deref())
  }

  /// The bytes as loaded (None: absent), for transactional expected-state
  /// checks and skip-unchanged-write decisions.
  pub fn original_bytes(&self) -> Option<&[u8]> {
    self.original.as_deref()
  }
}

/// For `update`: missing/malformed/older state regenerates from scratch, a
/// valid lockfile is returned for snapshot reuse, and a newer lockfile is
/// never overwritten.
pub fn load_for_update(path: &Path) -> Result<UpdateLockDocument> {
  let bytes = read_optional(path)?;
  let state = match &bytes {
    Some(bytes) => classify(bytes),
    None => LockState::Missing,
  };

  let reusable = match state {
    LockState::Newer { version } => bail!(
      "skillpm.lock version {version} is newer than this skillpm supports and will not be overwritten; upgrade skillpm"
    ),
    LockState::Valid { lockfile } => Some(lockfile),
    LockState::Missing | LockState::Malformed { .. } | LockState::Older { .. } => None,
  };

  Ok(UpdateLockDocument {
    reusable,
    path: path.to_path_buf(),
    original: bytes,
  })
}

fn check_matches_config(lockfile: &Lockfile, config: &Config) -> Result<()> {
  for (name, skill) in &config.skills {
    let Some(entry) = lockfile.skills.get(name) else {
      bail!("skillpm.lock has no entry for skill '{name}'; run `skillpm update`");
    };
    if entry.source != skill.source {
      bail!(
        "skillpm.lock entry for '{name}' was locked from a different source; run `skillpm update`"
      );
    }
    if entry.r#ref != skill.r#ref {
      bail!("skillpm.lock entry for '{name}' was locked at a different ref; run `skillpm update`");
    }
  }

  for name in lockfile.skills.keys() {
    if !config.skills.contains_key(name) {
      bail!("skillpm.lock has an entry for unknown skill '{name}'; run `skillpm update`");
    }
  }

  Ok(())
}

/// Deterministic: name-sorted via BTreeMap, fixed field order, no timestamps.
pub fn render(lockfile: &Lockfile) -> String {
  let mut doc = DocumentMut::new();
  doc.insert("version", value(lockfile.version));

  if !lockfile.skills.is_empty() {
    let mut skills = Table::new();
    skills.set_implicit(true);

    for (name, entry) in &lockfile.skills {
      let mut table = Table::new();
      table.insert("source", value(entry.source.as_str()));
      if let Some(r) = &entry.r#ref {
        table.insert("ref", value(r.as_str()));
      }
      if let Some(commit) = &entry.commit {
        table.insert("commit", value(commit.as_str()));
      }
      table.insert("content_hash", value(entry.content_hash.as_str()));
      skills.insert(name, Item::Table(table));
    }

    doc.insert("skills", Item::Table(skills));
  }

  doc.to_string()
}

/// Renders and validates: round-trips through the strict read path so the
/// write side can never persist state the read side would reject.
pub fn render_validated(lockfile: &Lockfile) -> Result<Vec<u8>> {
  let rendered = render(lockfile);

  match classify(rendered.as_bytes()) {
    LockState::Valid { lockfile: reparsed } if reparsed == *lockfile => {}
    LockState::Malformed { reason } => {
      bail!("refusing to write an invalid lockfile: {reason}")
    }
    other => bail!("refusing to write an invalid lockfile: {other:?}"),
  }

  Ok(rendered.into_bytes())
}

/// Atomic write at the logical global lock path via a temporary sibling.
/// Commands write through the transaction layer; tests use this to build
/// lock fixtures directly.
#[cfg_attr(not(test), allow(dead_code))]
pub fn write_atomic(path: &Path, lockfile: &Lockfile) -> Result<()> {
  let rendered = render_validated(lockfile)?;

  let parent = path
    .parent()
    .with_context(|| format!("lockfile {} has no parent directory", path.display()))?;

  let mut temp = tempfile::NamedTempFile::new_in(parent)
    .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
  temp.write_all(&rendered)?;

  if let Ok(metadata) = fs::metadata(path) {
    temp.as_file().set_permissions(metadata.permissions())?;
  }

  temp.as_file().sync_all()?;
  temp
    .persist(path)
    .with_context(|| format!("failed to write lockfile {}", path.display()))?;
  Ok(())
}

impl LockfileDocument {
  /// Pre-commit check: have the on-disk bytes changed since load?
  pub fn externally_modified(&self) -> Result<bool> {
    on_disk_bytes_changed(&self.path, self.original.as_deref())
  }

  /// The bytes as loaded, for transactional expected-state checks and
  /// skip-unchanged-write decisions.
  pub fn original_bytes(&self) -> Option<&[u8]> {
    self.original.as_deref()
  }
}

fn on_disk_bytes_changed(path: &Path, original: Option<&[u8]>) -> Result<bool> {
  match fs::read(path) {
    Ok(bytes) => Ok(original != Some(bytes.as_slice())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(original.is_some()),
    Err(error) => Err(error).with_context(|| format!("failed to read lockfile {}", path.display())),
  }
}

fn parse_v1(doc: &DocumentMut) -> Result<Lockfile> {
  for (key, _) in doc.iter() {
    if key != "version" && key != "skills" {
      bail!("unknown top-level field '{key}'");
    }
  }

  let mut skills = BTreeMap::new();

  if let Some(item) = doc.get("skills") {
    let Some(table) = item.as_table_like() else {
      bail!("'skills' must be a table");
    };

    for (name, item) in table.iter() {
      let entry =
        parse_entry(item).with_context(|| format!("invalid lock entry for skill '{name}'"))?;
      skills.insert(name.to_string(), entry);
    }
  }

  Ok(Lockfile {
    version: LOCK_VERSION,
    skills,
  })
}

fn parse_entry(item: &Item) -> Result<LockedSkill> {
  let Some(table) = item.as_table_like() else {
    bail!("entry must be a table");
  };

  for (key, _) in table.iter() {
    if !matches!(key, "source" | "ref" | "commit" | "content_hash") {
      bail!("unknown field '{key}'");
    }
  }

  let source = require_string(table, "source")?;
  let r#ref = optional_string(table, "ref")?;
  let commit = optional_string(table, "commit")?;
  let content_hash = require_string(table, "content_hash")?;

  let Some(hex) = content_hash.strip_prefix("sha256:") else {
    bail!("'content_hash' must start with 'sha256:'");
  };
  if !is_lower_hex(hex, 64) {
    bail!("'content_hash' must be sha256: plus 64 lowercase hex digits");
  }

  // the lockfile trusts the source spelling but knows github entries carry a
  // commit and local entries carry neither commit nor ref
  if source.starts_with("github:") {
    let Some(commit) = &commit else {
      bail!("GitHub entry is missing 'commit'");
    };
    if !is_lower_hex(commit, 40) {
      bail!("'commit' must be a full 40-character lowercase hex SHA");
    }
  } else {
    if commit.is_some() {
      bail!("local entry must not have 'commit'");
    }
    if r#ref.is_some() {
      bail!("local entry must not have 'ref'");
    }
  }

  Ok(LockedSkill {
    source,
    r#ref,
    commit,
    content_hash,
  })
}

fn require_string(table: &dyn toml_edit::TableLike, key: &str) -> Result<String> {
  let Some(item) = table.get(key) else {
    bail!("missing '{key}'");
  };
  let Some(text) = item.as_str() else {
    bail!("'{key}' must be a string");
  };
  if text.is_empty() {
    bail!("'{key}' must not be empty");
  }
  Ok(text.to_string())
}

fn optional_string(table: &dyn toml_edit::TableLike, key: &str) -> Result<Option<String>> {
  if table.get(key).is_none() {
    return Ok(None);
  }
  require_string(table, key).map(Some)
}

fn is_lower_hex(text: &str, length: usize) -> bool {
  text.len() == length
    && text
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::Skill;

  const HASH_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const HASH_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
  const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

  fn github_entry() -> LockedSkill {
    LockedSkill {
      source: "github:anthropics/skills/frontend-design".into(),
      r#ref: Some("main".into()),
      commit: Some(COMMIT.into()),
      content_hash: HASH_A.into(),
    }
  }

  fn local_entry() -> LockedSkill {
    LockedSkill {
      source: "skills/my-local-skill".into(),
      r#ref: None,
      commit: None,
      content_hash: HASH_B.into(),
    }
  }

  fn mixed_lockfile() -> Lockfile {
    let mut lockfile = Lockfile::empty();
    // inserted out of name order to prove render sorts
    lockfile
      .skills
      .insert("my-local-skill".into(), local_entry());
    lockfile
      .skills
      .insert("frontend-design".into(), github_entry());
    lockfile
  }

  fn config_matching_mixed() -> Config {
    let mut skills = BTreeMap::new();
    skills.insert(
      "frontend-design".into(),
      Skill {
        source: "github:anthropics/skills/frontend-design".into(),
        r#ref: Some("main".into()),
        targets: vec![PathBuf::from(".claude/skills/frontend-design")],
      },
    );
    skills.insert(
      "my-local-skill".into(),
      Skill {
        source: "skills/my-local-skill".into(),
        r#ref: None,
        targets: vec![PathBuf::from(".claude/skills/my-local-skill")],
      },
    );
    Config { version: 1, skills }
  }

  fn write_lock(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("skillpm.lock");
    fs::write(&path, contents).unwrap();
    path
  }

  #[test]
  fn golden_empty() {
    assert_eq!(render(&Lockfile::empty()), "version = 1\n");
  }

  #[test]
  fn golden_mixed() {
    let expected = format!(
      r#"version = 1

[skills.frontend-design]
source = "github:anthropics/skills/frontend-design"
ref = "main"
commit = "{COMMIT}"
content_hash = "{HASH_A}"

[skills.my-local-skill]
source = "skills/my-local-skill"
content_hash = "{HASH_B}"
"#
    );
    assert_eq!(render(&mixed_lockfile()), expected);
  }

  #[test]
  fn golden_github_without_ref_omits_the_ref_line() {
    let mut lockfile = Lockfile::empty();
    lockfile.skills.insert(
      "x".into(),
      LockedSkill {
        r#ref: None,
        ..github_entry()
      },
    );

    let rendered = render(&lockfile);
    assert!(!rendered.contains("ref ="));
    assert!(rendered.contains("commit ="));
  }

  #[test]
  fn render_is_deterministic_and_round_trips() {
    let lockfile = mixed_lockfile();
    let first = render(&lockfile);
    assert_eq!(first, render(&lockfile));

    let LockState::Valid { lockfile: reparsed } = classify(first.as_bytes()) else {
      panic!("rendered lockfile must classify as valid");
    };
    assert_eq!(reparsed, lockfile);
  }

  #[test]
  fn state_classification() {
    let temp = tempfile::tempdir().unwrap();

    let missing = temp.path().join("skillpm.lock");
    assert!(matches!(
      read_lock_state(&missing).unwrap(),
      LockState::Missing
    ));

    type StateCheck = fn(&LockState) -> bool;
    let cases: &[(&str, StateCheck)] = &[
      ("version = 0\n", |s| {
        matches!(s, LockState::Older { version: 0 })
      }),
      ("version = 2\n", |s| {
        matches!(s, LockState::Newer { version: 2 })
      }),
      // newer lockfiles are not schema-checked; unknown fields are expected
      ("version = 2\nfuture_field = true\n", |s| {
        matches!(s, LockState::Newer { version: 2 })
      }),
      ("not toml [", |s| matches!(s, LockState::Malformed { .. })),
      ("", |s| matches!(s, LockState::Malformed { .. })),
      ("version = \"1\"\n", |s| {
        matches!(s, LockState::Malformed { .. })
      }),
      ("version = 1\nextra = 1\n", |s| {
        matches!(s, LockState::Malformed { .. })
      }),
    ];

    for (contents, check) in cases {
      let path = write_lock(temp.path(), contents);
      let state = read_lock_state(&path).unwrap();
      assert!(
        check(&state),
        "unexpected state for {contents:?}: {state:?}"
      );
    }
  }

  #[test]
  fn schema_rejects_invalid_entries() {
    let cases: &[(&str, &str)] = &[
      ("source = \"s/x\"\n", "missing 'content_hash'"),
      (
        &format!("source = \"s/x\"\ncontent_hash = \"{HASH_A}\"\ncommit = \"{COMMIT}\"\n"),
        "local entry must not have 'commit'",
      ),
      (
        &format!("source = \"s/x\"\nref = \"main\"\ncontent_hash = \"{HASH_A}\"\n"),
        "local entry must not have 'ref'",
      ),
      (
        &format!("source = \"github:a/b\"\ncontent_hash = \"{HASH_A}\"\n"),
        "missing 'commit'",
      ),
      (
        &format!("source = \"github:a/b\"\ncommit = \"abc\"\ncontent_hash = \"{HASH_A}\"\n"),
        "40-character",
      ),
      (
        &format!(
          "source = \"github:a/b\"\ncommit = \"{}\"\ncontent_hash = \"{HASH_A}\"\n",
          COMMIT.to_uppercase()
        ),
        "lowercase",
      ),
      (
        &format!("source = \"github:a/b\"\ncommit = \"{COMMIT}\"\ncontent_hash = \"abc\"\n"),
        "sha256:",
      ),
      (
        &format!("source = \"github:a/b\"\ncommit = \"{COMMIT}\"\ncontent_hash = \"sha256:zz\"\n"),
        "64 lowercase hex",
      ),
      (
        &format!(
          "source = \"github:a/b\"\ncommit = \"{COMMIT}\"\ncontent_hash = \"{HASH_A}\"\nnote = \"x\"\n"
        ),
        "unknown field 'note'",
      ),
    ];

    for (entry, expected) in cases {
      let contents = format!("version = 1\n[skills.x]\n{entry}");
      let LockState::Malformed { reason } = classify(contents.as_bytes()) else {
        panic!("expected malformed for {contents:?}");
      };
      assert!(
        reason.contains(expected),
        "expected '{expected}' for {contents:?}, got: {reason}"
      );
    }
  }

  #[test]
  fn require_fresh_accepts_a_matching_lock() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_lock(temp.path(), &render(&mixed_lockfile()));

    let doc = require_fresh(&path, &config_matching_mixed()).unwrap();
    assert_eq!(doc.lockfile, mixed_lockfile());
    assert!(!doc.externally_modified().unwrap());
  }

  #[test]
  fn target_only_config_edits_stay_fresh() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_lock(temp.path(), &render(&mixed_lockfile()));

    let mut config = config_matching_mixed();
    config.skills.get_mut("frontend-design").unwrap().targets =
      vec![PathBuf::from("elsewhere/frontend-design")];

    require_fresh(&path, &config).unwrap();
  }

  #[test]
  fn require_fresh_rejects_every_non_fresh_state() {
    let temp = tempfile::tempdir().unwrap();
    let config = config_matching_mixed();

    // missing file
    let missing = temp.path().join("skillpm.lock");
    let error = require_fresh(&missing, &config).unwrap_err();
    assert!(error.to_string().contains("run `skillpm update`"));

    // malformed / older / newer
    for (contents, expected) in [
      ("nope [", "malformed"),
      ("version = 0\n", "older"),
      ("version = 9\n", "upgrade skillpm"),
    ] {
      let path = write_lock(temp.path(), contents);
      let error = require_fresh(&path, &config).unwrap_err();
      assert!(
        error.to_string().contains(expected),
        "expected '{expected}', got: {error}"
      );
    }

    // missing entry
    let mut one_short = mixed_lockfile();
    one_short.skills.remove("my-local-skill");
    let path = write_lock(temp.path(), &render(&one_short));
    let error = require_fresh(&path, &config).unwrap_err();
    assert!(
      error
        .to_string()
        .contains("no entry for skill 'my-local-skill'")
    );

    // extra entry
    let mut extra = mixed_lockfile();
    extra.skills.insert("stray".into(), local_entry());
    let path = write_lock(temp.path(), &render(&extra));
    let error = require_fresh(&path, &config).unwrap_err();
    assert!(error.to_string().contains("unknown skill 'stray'"));

    // source mismatch
    let mut moved = mixed_lockfile();
    moved.skills.get_mut("my-local-skill").unwrap().source = "elsewhere/skill".into();
    let path = write_lock(temp.path(), &render(&moved));
    let error = require_fresh(&path, &config).unwrap_err();
    assert!(error.to_string().contains("different source"));

    // ref mismatch, including Some vs None
    let mut retagged = mixed_lockfile();
    retagged.skills.get_mut("frontend-design").unwrap().r#ref = Some("v2".into());
    let path = write_lock(temp.path(), &render(&retagged));
    assert!(
      require_fresh(&path, &config)
        .unwrap_err()
        .to_string()
        .contains("different ref")
    );

    let mut unrefed = mixed_lockfile();
    unrefed.skills.get_mut("frontend-design").unwrap().r#ref = None;
    let path = write_lock(temp.path(), &render(&unrefed));
    assert!(
      require_fresh(&path, &config)
        .unwrap_err()
        .to_string()
        .contains("different ref")
    );
  }

  #[test]
  fn update_can_regenerate_everything_except_newer() {
    let temp = tempfile::tempdir().unwrap();

    let missing = temp.path().join("skillpm.lock");
    assert_eq!(load_for_update(&missing).unwrap().reusable, None);

    for contents in ["nope [", "version = 0\n"] {
      let path = write_lock(temp.path(), contents);
      assert_eq!(load_for_update(&path).unwrap().reusable, None);
    }

    let valid = write_lock(temp.path(), &render(&mixed_lockfile()));
    assert_eq!(
      load_for_update(&valid).unwrap().reusable,
      Some(mixed_lockfile())
    );

    let newer = write_lock(temp.path(), "version = 3\n");
    let error = load_for_update(&newer).unwrap_err();
    assert!(error.to_string().contains("will not be overwritten"));
  }

  #[test]
  fn update_document_detects_external_modification() {
    let temp = tempfile::tempdir().unwrap();

    // even malformed original bytes participate in the check
    let path = write_lock(temp.path(), "nope [");
    let doc = load_for_update(&path).unwrap();
    assert!(!doc.externally_modified().unwrap());

    fs::write(&path, "version = 1\n").unwrap();
    assert!(doc.externally_modified().unwrap());

    // a lock that appears where none existed is also a modification
    let absent = temp.path().join("other.lock");
    let doc = load_for_update(&absent).unwrap();
    assert!(!doc.externally_modified().unwrap());

    fs::write(&absent, "version = 1\n").unwrap();
    assert!(doc.externally_modified().unwrap());
  }

  #[test]
  fn write_atomic_refuses_invalid_models() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_lock(temp.path(), "version = 1\n");

    let entry_with = |entry: LockedSkill| Lockfile {
      version: LOCK_VERSION,
      skills: BTreeMap::from([("x".to_string(), entry)]),
    };

    let bad_hash = entry_with(LockedSkill {
      content_hash: "sha256:short".into(),
      ..local_entry()
    });
    let local_with_commit = entry_with(LockedSkill {
      commit: Some(COMMIT.into()),
      ..local_entry()
    });
    let unsupported_version = Lockfile {
      version: 2,
      skills: BTreeMap::new(),
    };

    for lockfile in [&bad_hash, &local_with_commit, &unsupported_version] {
      let error = write_atomic(&path, lockfile).unwrap_err();
      assert!(
        error.to_string().contains("refusing to write"),
        "unexpected error: {error}"
      );
    }

    assert_eq!(
      fs::read_to_string(&path).unwrap(),
      "version = 1\n",
      "disk must be untouched"
    );
  }

  #[test]
  fn atomic_write_replaces_content_and_leaves_no_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("skillpm.lock");

    write_atomic(&path, &Lockfile::empty()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "version = 1\n");

    write_atomic(&path, &mixed_lockfile()).unwrap();
    assert_eq!(
      fs::read_to_string(&path).unwrap(),
      render(&mixed_lockfile())
    );

    let entries: Vec<_> = fs::read_dir(temp.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("skillpm.lock")]);
  }

  #[test]
  fn external_modification_is_detected() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_lock(temp.path(), &render(&mixed_lockfile()));

    let doc = require_fresh(&path, &config_matching_mixed()).unwrap();
    assert!(!doc.externally_modified().unwrap());

    fs::write(&path, "version = 1\n").unwrap();
    assert!(doc.externally_modified().unwrap());
  }
}
