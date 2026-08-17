use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use yaml_rust2::{Yaml, YamlLoader};

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
  pub name: String,
  pub description: String,
}

/// Reads and validates `<dir>/SKILL.md`. Read-only: the original bytes are
/// never rewritten. The directory's own name is deliberately never consulted.
pub fn load_skill_metadata(dir: &Path) -> Result<SkillMetadata> {
  let path = dir.join("SKILL.md");

  let metadata = match fs::symlink_metadata(&path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      bail!("{} has no SKILL.md", dir.display());
    }
    Err(error) => {
      return Err(error).with_context(|| format!("failed to read {}", path.display()));
    }
  };
  // symlinked or otherwise non-regular SKILL.md is rejected, not followed
  if !metadata.is_file() {
    bail!("{} must be a regular file", path.display());
  }

  let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
  let text = str::from_utf8(&bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;

  parse_frontmatter(text).with_context(|| format!("invalid {}", path.display()))
}

fn parse_frontmatter(text: &str) -> Result<SkillMetadata> {
  let yaml = extract_frontmatter(text)?;
  let docs = YamlLoader::load_from_str(yaml).context("frontmatter is not valid YAML")?;
  let Some(doc) = docs.first() else {
    bail!("frontmatter is empty");
  };
  let Some(mapping) = doc.as_hash() else {
    bail!("frontmatter must be a YAML mapping");
  };

  let name = required_string(mapping, "name")?;
  validate_name(&name)?;

  let description = required_string(mapping, "description")?;
  validate_description(&description)?;

  // other frontmatter fields are permitted and ignored
  Ok(SkillMetadata { name, description })
}

fn extract_frontmatter(text: &str) -> Result<&str> {
  let Some(rest) = text
    .strip_prefix("---\n")
    .or_else(|| text.strip_prefix("---\r\n"))
  else {
    bail!("SKILL.md must start with '---' YAML frontmatter");
  };

  let mut offset = 0;
  for line in rest.split_inclusive('\n') {
    if line.trim_end_matches('\n').trim_end_matches('\r') == "---" {
      return Ok(&rest[..offset]);
    }
    offset += line.len();
  }

  bail!("frontmatter is missing its closing '---'");
}

fn required_string(mapping: &yaml_rust2::yaml::Hash, key: &str) -> Result<String> {
  let Some(value) = mapping.get(&Yaml::String(key.to_string())) else {
    bail!("frontmatter is missing '{key}'");
  };
  let Some(text) = value.as_str() else {
    bail!("frontmatter '{key}' must be a string");
  };
  Ok(text.to_string())
}

fn validate_name(name: &str) -> Result<()> {
  if name.is_empty() || name.len() > MAX_NAME_LENGTH {
    bail!(
      "skill name must be 1-{MAX_NAME_LENGTH} characters, got {}",
      name.len()
    );
  }

  let valid_chars = name
    .bytes()
    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
  if !valid_chars {
    bail!("skill name '{name}' may only contain lowercase ASCII letters, digits, and hyphens");
  }

  if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
    bail!("skill name '{name}' may not have leading, trailing, or consecutive hyphens");
  }

  Ok(())
}

fn validate_description(description: &str) -> Result<()> {
  if description.trim().is_empty() {
    bail!("skill description must not be empty");
  }

  let chars = description.chars().count();
  if chars > MAX_DESCRIPTION_CHARS {
    bail!("skill description must be at most {MAX_DESCRIPTION_CHARS} characters, got {chars}");
  }

  Ok(())
}

/// The `[skills.<key>]` config key must equal the validated frontmatter name.
/// There is no alias mechanism.
pub fn ensure_key_matches(key: &str, skill_name: &str) -> Result<()> {
  if key != skill_name {
    bail!("config key '{key}' does not match skill name '{skill_name}' from SKILL.md");
  }
  Ok(())
}

/// Every target's final path component must equal the skill name.
pub fn ensure_target_matches(target: &Path, skill_name: &str) -> Result<()> {
  let basename = target.file_name().and_then(|name| name.to_str());
  if basename != Some(skill_name) {
    bail!(
      "target '{}' must end in the skill name '{skill_name}'",
      target.display()
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  const VALID: &str = "---\nname: frontend-design\ndescription: Design frontend UIs.\nlicense: MIT\nallowed-tools: [Bash]\n---\n\n# Frontend design\n";

  fn write_skill(contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("SKILL.md"), contents).unwrap();
    let dir = temp.path().to_path_buf();
    (temp, dir)
  }

  #[test]
  fn valid_skill_parses_and_ignores_extra_fields() {
    let (_temp, dir) = write_skill(VALID.as_bytes());

    let metadata = load_skill_metadata(&dir).unwrap();
    assert_eq!(
      metadata,
      SkillMetadata {
        name: "frontend-design".into(),
        description: "Design frontend UIs.".into(),
      }
    );
  }

  #[test]
  fn original_bytes_are_not_rewritten() {
    let (_temp, dir) = write_skill(VALID.as_bytes());

    load_skill_metadata(&dir).unwrap();
    assert_eq!(fs::read(dir.join("SKILL.md")).unwrap(), VALID.as_bytes());
  }

  #[test]
  fn missing_skill_md_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let error = load_skill_metadata(temp.path()).unwrap_err();
    assert!(error.to_string().contains("no SKILL.md"));
  }

  #[test]
  fn non_regular_skill_md_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("SKILL.md")).unwrap();

    let error = load_skill_metadata(temp.path()).unwrap_err();
    assert!(error.to_string().contains("regular file"));
  }

  #[test]
  #[cfg(unix)]
  fn symlinked_skill_md_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("real.md"), VALID).unwrap();
    std::os::unix::fs::symlink(temp.path().join("real.md"), temp.path().join("SKILL.md")).unwrap();

    let error = load_skill_metadata(temp.path()).unwrap_err();
    assert!(error.to_string().contains("regular file"));
  }

  #[test]
  fn non_utf8_skill_md_is_rejected() {
    let (_temp, dir) = write_skill(b"---\nname: x\xff\n---\n");

    let error = load_skill_metadata(&dir).unwrap_err();
    assert!(format!("{error:#}").contains("not UTF-8"));
  }

  #[test]
  fn malformed_frontmatter_is_rejected() {
    let cases: &[(&str, &str)] = &[
      ("# no frontmatter\n", "must start with '---'"),
      ("---\nname: x\n", "missing its closing '---'"),
      ("---\n---\n", "frontmatter is empty"),
      ("---\n[not: yaml\n---\n", "not valid YAML"),
      ("---\n- just\n- a\n- list\n---\n", "must be a YAML mapping"),
      ("---\ndescription: something\n---\n", "missing 'name'"),
      (
        "---\nname: [a, b]\ndescription: x\n---\n",
        "'name' must be a string",
      ),
      ("---\nname: valid-name\n---\n", "missing 'description'"),
      (
        "---\nname: valid-name\ndescription: {a: b}\n---\n",
        "'description' must be a string",
      ),
    ];

    for (contents, expected) in cases {
      let (_temp, dir) = write_skill(contents.as_bytes());
      let error = format!("{:#}", load_skill_metadata(&dir).unwrap_err());
      assert!(
        error.contains(expected),
        "expected '{expected}' for {contents:?}, got: {error}"
      );
    }
  }

  #[test]
  fn name_constraints() {
    for valid in ["a", "frontend-design", "skill2", "a-b-c", &"a".repeat(64)] {
      assert!(validate_name(valid).is_ok(), "name: {valid}");
    }

    for invalid in [
      "",
      "Frontend",
      "front_end",
      "front end",
      "-lead",
      "trail-",
      "double--hyphen",
      "émoji",
      &"a".repeat(65),
    ] {
      assert!(validate_name(invalid).is_err(), "name: {invalid:?}");
    }
  }

  #[test]
  fn description_constraints() {
    assert!(validate_description("Designs frontends.").is_ok());
    assert!(validate_description(&"x".repeat(1024)).is_ok());

    assert!(validate_description("").is_err());
    assert!(validate_description("   \n  ").is_err());
    assert!(validate_description(&"x".repeat(1025)).is_err());

    // the limit counts characters, not bytes
    assert!(validate_description(&"é".repeat(1024)).is_ok());
  }

  #[test]
  fn key_and_target_equality() {
    assert!(ensure_key_matches("frontend-design", "frontend-design").is_ok());
    assert!(ensure_key_matches("other-key", "frontend-design").is_err());

    assert!(
      ensure_target_matches(
        Path::new(".claude/skills/frontend-design"),
        "frontend-design"
      )
      .is_ok()
    );
    assert!(ensure_target_matches(Path::new(".claude/skills/renamed"), "frontend-design").is_err());
    assert!(ensure_target_matches(Path::new("/"), "frontend-design").is_err());
  }
}
