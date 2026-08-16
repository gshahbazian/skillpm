use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// A minimal reader for `git archive --format=tar` output: ustar fields plus
/// the pax (`x`/`g`) and GNU (`L`/`K`) long-name extensions git emits.
/// Checksums are not verified; the bytes come from our own git subprocess.
#[derive(Debug, PartialEq)]
struct TarEntry<'a> {
  path: String,
  kind: TarKind,
  data: &'a [u8],
}

#[derive(Debug, PartialEq)]
enum TarKind {
  File { executable: bool },
  Dir,
  Symlink { destination: String },
}

/// Unpacks a tar into `destination`, keeping only entries under
/// `strip_prefix` (with the prefix removed). Every entry path is validated
/// against traversal before any filesystem write. Returns the number of
/// entries written.
pub fn unpack_tar(bytes: &[u8], destination: &Path, strip_prefix: Option<&str>) -> Result<usize> {
  let entries = parse_tar(bytes)?;

  fs::create_dir_all(destination)
    .with_context(|| format!("failed to create {}", destination.display()))?;

  let mut written = 0usize;
  for entry in entries {
    let Some(components) = select_components(&entry.path, strip_prefix)? else {
      continue; // prefix ancestors and unrelated paths
    };

    let mut target = destination.to_path_buf();
    for component in &components {
      target.push(component);
    }

    match &entry.kind {
      TarKind::Dir => {
        fs::create_dir_all(&target)
          .with_context(|| format!("failed to create {}", target.display()))?;
      }
      TarKind::File { executable } => {
        if let Some(parent) = target.parent() {
          fs::create_dir_all(parent)?;
        }
        fs::write(&target, entry.data)
          .with_context(|| format!("failed to write {}", target.display()))?;
        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          let mode = if *executable { 0o755 } else { 0o644 };
          fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
      }
      TarKind::Symlink { destination: dest } => {
        if dest.is_empty() {
          bail!("symlink entry '{}' has an empty destination", entry.path);
        }
        // link safety (absolute/escaping/loops) is enforced by the snapshot
        // scan that follows every unpack; creating a link never follows it
        #[cfg(unix)]
        std::os::unix::fs::symlink(dest, &target)
          .with_context(|| format!("failed to create symlink {}", target.display()))?;
        #[cfg(not(unix))]
        bail!(
          "cannot create symlink {} on this platform",
          target.display()
        );
      }
    }
    written += 1;
  }

  Ok(written)
}

/// Validates and splits an entry path; applies the prefix filter. `None`
/// means the entry falls outside the selected prefix and is skipped.
fn select_components(raw: &str, strip_prefix: Option<&str>) -> Result<Option<Vec<String>>> {
  let trimmed = raw.trim_end_matches('/');
  if trimmed.is_empty() || raw.starts_with('/') {
    bail!("archive entry '{raw}' has an unsupported absolute or empty path");
  }

  let components: Vec<&str> = trimmed.split('/').collect();
  for component in &components {
    if component.is_empty() || *component == "." || *component == ".." {
      bail!("archive entry '{raw}' attempts path traversal");
    }
  }

  let Some(prefix) = strip_prefix else {
    return Ok(Some(components.iter().map(|c| c.to_string()).collect()));
  };

  let prefix_parts: Vec<&str> = prefix.split('/').collect();
  if components.len() <= prefix_parts.len() || components[..prefix_parts.len()] != prefix_parts {
    return Ok(None);
  }

  Ok(Some(
    components[prefix_parts.len()..]
      .iter()
      .map(|c| c.to_string())
      .collect(),
  ))
}

fn parse_tar(bytes: &[u8]) -> Result<Vec<TarEntry<'_>>> {
  let mut entries = Vec::new();
  let mut pos = 0usize;
  // pax/GNU extensions describe the NEXT entry
  let mut pending_path: Option<String> = None;
  let mut pending_link: Option<String> = None;

  while pos + 512 <= bytes.len() {
    let header = &bytes[pos..pos + 512];
    pos += 512;

    if header.iter().all(|byte| *byte == 0) {
      break; // end-of-archive marker
    }

    let size = parse_octal(&header[124..136]).context("invalid tar size field")?;
    let data_len = usize::try_from(size).context("tar entry too large")?;
    if pos + data_len > bytes.len() {
      bail!("truncated tar archive");
    }
    let data = &bytes[pos..pos + data_len];
    pos += data_len.div_ceil(512) * 512;

    let typeflag = header[156];
    match typeflag {
      b'x' => {
        let (path, link) = parse_pax(data)?;
        pending_path = path.or(pending_path);
        pending_link = link.or(pending_link);
      }
      b'g' => {} // pax global header; git stores the commit comment here
      b'L' => pending_path = Some(nul_terminated_string(data)?),
      b'K' => pending_link = Some(nul_terminated_string(data)?),
      b'0' | 0 => {
        let mode = parse_octal(&header[100..108]).context("invalid tar mode field")?;
        entries.push(TarEntry {
          path: entry_path(header, pending_path.take())?,
          kind: TarKind::File {
            executable: mode & 0o111 != 0,
          },
          data,
        });
        pending_link = None;
      }
      b'5' => {
        entries.push(TarEntry {
          path: entry_path(header, pending_path.take())?,
          kind: TarKind::Dir,
          data: &[],
        });
        pending_link = None;
      }
      b'2' => {
        let destination = match pending_link.take() {
          Some(link) => link,
          None => header_field(&header[157..257])?.to_string(),
        };
        entries.push(TarEntry {
          path: entry_path(header, pending_path.take())?,
          kind: TarKind::Symlink { destination },
          data: &[],
        });
      }
      other => bail!(
        "unsupported tar entry type '{}'; only files, directories, and symlinks are allowed",
        other as char
      ),
    }
  }

  Ok(entries)
}

fn entry_path(header: &[u8], pending: Option<String>) -> Result<String> {
  if let Some(path) = pending {
    return Ok(path);
  }

  let name = header_field(&header[0..100])?;
  let prefix = if &header[257..262] == b"ustar" {
    header_field(&header[345..500])?
  } else {
    ""
  };

  if prefix.is_empty() {
    Ok(name.to_string())
  } else {
    Ok(format!("{prefix}/{name}"))
  }
}

fn header_field(bytes: &[u8]) -> Result<&str> {
  let end = bytes
    .iter()
    .position(|byte| *byte == 0)
    .unwrap_or(bytes.len());
  str::from_utf8(&bytes[..end]).context("non-UTF-8 tar header field")
}

fn nul_terminated_string(data: &[u8]) -> Result<String> {
  let end = data
    .iter()
    .position(|byte| *byte == 0)
    .unwrap_or(data.len());
  Ok(
    str::from_utf8(&data[..end])
      .context("non-UTF-8 tar extension data")?
      .to_string(),
  )
}

fn parse_octal(bytes: &[u8]) -> Result<u64> {
  let text = header_field(bytes)?.trim();
  if text.is_empty() {
    return Ok(0);
  }
  u64::from_str_radix(text, 8).with_context(|| format!("invalid octal field '{text}'"))
}

/// Pax data is a sequence of "<byte-len> <key>=<value>\n" records.
fn parse_pax(data: &[u8]) -> Result<(Option<String>, Option<String>)> {
  let mut path = None;
  let mut link = None;
  let mut rest = data;

  while !rest.is_empty() {
    let space = rest
      .iter()
      .position(|byte| *byte == b' ')
      .context("malformed pax record")?;
    let length: usize = str::from_utf8(&rest[..space])
      .ok()
      .and_then(|text| text.parse().ok())
      .context("malformed pax record length")?;
    if length <= space + 1 || length > rest.len() {
      bail!("malformed pax record length");
    }

    // strip "<len> " and the trailing newline
    let record = &rest[space + 1..length - 1];
    let record = str::from_utf8(record).context("non-UTF-8 pax record")?;
    if let Some((key, value)) = record.split_once('=') {
      match key {
        "path" => path = Some(value.to_string()),
        "linkpath" => link = Some(value.to_string()),
        _ => {}
      }
    }

    rest = &rest[length..];
  }

  Ok((path, link))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn header(name: &str, size: usize, typeflag: u8, mode: u32, link: &str) -> Vec<u8> {
    let mut header = vec![0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    header[156] = typeflag;
    header[157..157 + link.len()].copy_from_slice(link.as_bytes());
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header
  }

  fn entry(name: &str, typeflag: u8, mode: u32, link: &str, data: &[u8]) -> Vec<u8> {
    let mut out = header(name, data.len(), typeflag, mode, link);
    out.extend_from_slice(data);
    out.resize(512 + data.len().div_ceil(512) * 512, 0);
    out
  }

  fn tar(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = parts.concat();
    out.extend_from_slice(&[0u8; 1024]);
    out
  }

  #[test]
  #[cfg(unix)]
  fn unpacks_files_dirs_and_symlinks_with_prefix_strip() {
    let bytes = tar(&[
      entry("skills/", b'5', 0o755, "", b""),
      entry("skills/x/", b'5', 0o755, "", b""),
      entry("skills/x/SKILL.md", b'0', 0o644, "", b"content"),
      entry("skills/x/run.sh", b'0', 0o755, "", b"#!/bin/sh\n"),
      entry("skills/x/link", b'2', 0o777, "SKILL.md", b""),
      entry("skills/other/ignored.md", b'0', 0o644, "", b"outside"),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    let written = unpack_tar(&bytes, &dest, Some("skills/x")).unwrap();

    assert_eq!(
      written, 3,
      "prefix ancestors and outside entries are skipped"
    );
    assert_eq!(
      fs::read_to_string(dest.join("SKILL.md")).unwrap(),
      "content"
    );
    assert_eq!(
      fs::read_link(dest.join("link")).unwrap(),
      std::path::PathBuf::from("SKILL.md")
    );

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(dest.join("run.sh"))
      .unwrap()
      .permissions()
      .mode()
      & 0o111;
    assert_ne!(mode, 0, "executable bit survives");
    assert!(!dest.join("ignored.md").exists());
  }

  #[test]
  fn unpacks_without_prefix_for_repo_root_skills() {
    let bytes = tar(&[
      entry("SKILL.md", b'0', 0o644, "", b"root"),
      entry("docs/", b'5', 0o755, "", b""),
      entry("docs/a.md", b'0', 0o644, "", b"a"),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    assert_eq!(unpack_tar(&bytes, &dest, None).unwrap(), 3);
    assert_eq!(fs::read_to_string(dest.join("docs/a.md")).unwrap(), "a");
  }

  #[test]
  fn traversal_and_absolute_entries_are_rejected() {
    let hostile: &[(&str, u8, &str)] = &[
      ("../evil", b'0', ""),
      ("a/../../evil", b'0', ""),
      ("a/./evil", b'0', ""),
      ("/etc/passwd", b'0', ""),
      ("a//b", b'0', ""),
    ];

    for (name, typeflag, link) in hostile {
      let bytes = tar(&[entry(name, *typeflag, 0o644, link, b"x")]);
      let temp = tempfile::tempdir().unwrap();
      let error = unpack_tar(&bytes, &temp.path().join("out"), None).unwrap_err();
      assert!(
        format!("{error:#}").contains("traversal") || format!("{error:#}").contains("absolute"),
        "expected rejection for {name}: {error:#}"
      );
    }
  }

  /// "<byte-len> <key>=<value>\n" where byte-len counts the whole record,
  /// including its own digits — hence the fixpoint loop.
  fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut length = body.len() + 1;
    while length.to_string().len() + body.len() != length {
      length = length.to_string().len() + body.len();
    }
    format!("{length}{body}")
  }

  #[test]
  fn pax_long_paths_are_honored_and_still_checked() {
    // pax path record overrides the header name
    let long = format!("skills/x/{}.md", "n".repeat(120));
    let record = pax_record("path", &long);
    let bytes = tar(&[
      entry("ignored", b'x', 0o644, "", record.as_bytes()),
      entry("short", b'0', 0o644, "", b"data"),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    unpack_tar(&bytes, &dest, Some("skills/x")).unwrap();
    assert!(dest.join(format!("{}.md", "n".repeat(120))).exists());

    // a hostile pax path is rejected like any other
    let evil = pax_record("path", "../evil");
    let bytes = tar(&[
      entry("ignored", b'x', 0o644, "", evil.as_bytes()),
      entry("short", b'0', 0o644, "", b"data"),
    ]);
    let temp = tempfile::tempdir().unwrap();
    assert!(unpack_tar(&bytes, &temp.path().join("out"), None).is_err());
  }

  #[test]
  fn gnu_longname_records_are_honored() {
    let long = format!("{}.md", "g".repeat(150));
    let bytes = tar(&[
      entry("ignored", b'L', 0o644, "", long.as_bytes()),
      entry("short", b'0', 0o644, "", b"data"),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    unpack_tar(&bytes, &dest, None).unwrap();
    assert!(dest.join(&long).exists());
  }

  #[test]
  fn unsupported_entry_types_are_rejected() {
    // '6' is a FIFO
    let bytes = tar(&[entry("pipe", b'6', 0o644, "", b"")]);
    let temp = tempfile::tempdir().unwrap();
    let error = unpack_tar(&bytes, &temp.path().join("out"), None).unwrap_err();
    assert!(error.to_string().contains("unsupported tar entry type"));
  }

  #[test]
  fn truncated_archives_are_rejected() {
    let mut bytes = tar(&[entry("file", b'0', 0o644, "", &[b'x'; 600])]);
    bytes.truncate(700); // the header promises 600 data bytes; only 188 remain
    let temp = tempfile::tempdir().unwrap();
    let error = unpack_tar(&bytes, &temp.path().join("out"), None).unwrap_err();
    assert!(format!("{error:#}").contains("truncated"));
  }

  #[test]
  fn global_pax_headers_are_skipped() {
    // git emits a 'g' header carrying the commit id as a comment
    let comment = "30 comment=0123456789abcdef01234\n";
    let bytes = tar(&[
      entry("pax_global_header", b'g', 0o644, "", comment.as_bytes()),
      entry("file", b'0', 0o644, "", b"data"),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    assert_eq!(unpack_tar(&bytes, &dest, None).unwrap(), 1);
    assert!(dest.join("file").exists());
    assert!(!dest.join("pax_global_header").exists());
  }
}
