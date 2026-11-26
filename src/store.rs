//! Persistent `.musman.toml` configuration files.
//!
//! Two kinds of config exist:
//!
//! - the *link config* at `<source>/.musman.toml`, describing the library:
//!   its managed formats and the (relative) roots of the derived link trees;
//! - the *sync config* at `<target>/.musman.toml`, describing one view of the
//!   library: which selection to copy and whether stale files may be removed.
//!
//! Both files are hidden, human-editable TOML. Link config paths are stored
//! relative to the source directory so a library can be mounted elsewhere and
//! the same config keeps working.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::SyncSelection;

/// File name used for both the link and the sync config.
pub const CONFIG_FILE_NAME: &str = ".musman.toml";

/// Saved link settings, stored at `<source>/.musman.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLink {
    /// Managed formats, best first.
    pub formats: Vec<String>,
    /// Per-format link tree root, relative to the source directory.
    pub formats_root: String,
    /// Best-quality link tree root, relative to the source directory.
    pub max_qual_root: String,
}

/// Saved sync settings, stored at `<target>/.musman.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSync {
    /// Either `"best"` or `"only:<ext>"`.
    pub selection: String,
    /// Keep files the selected view no longer contains.
    #[serde(default)]
    pub no_delete: bool,
}

pub fn link_config_path(source: &Path) -> PathBuf {
    source.join(CONFIG_FILE_NAME)
}

pub fn sync_config_path(target: &Path) -> PathBuf {
    target.join(CONFIG_FILE_NAME)
}

pub fn load_link(source: &Path) -> Result<Option<SavedLink>> {
    let saved = load_config(&link_config_path(source), "link")?;
    if let Some(saved) = &saved {
        validate_link(saved)?;
    }
    Ok(saved)
}

pub fn save_link(source: &Path, saved: &SavedLink) -> Result<()> {
    validate_link(saved)?;
    save_config(&link_config_path(source), saved)
}

pub fn load_sync(target: &Path) -> Result<Option<SavedSync>> {
    let saved: Option<SavedSync> = load_config(&sync_config_path(target), "sync")?;
    if let Some(saved) = &saved {
        parse_selection(&saved.selection)?;
    }
    Ok(saved)
}

pub fn save_sync(target: &Path, saved: &SavedSync) -> Result<()> {
    parse_selection(&saved.selection)?;
    save_config(&sync_config_path(target), saved)
}

fn load_config<T: serde::de::DeserializeOwned>(path: &Path, kind: &str) -> Result<Option<T>> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("read {kind} config {}", path.display()));
        }
    };
    let value = toml::from_str::<T>(&data)
        .with_context(|| format!("parse {kind} config {}", path.display()))?;
    Ok(Some(value))
}

fn save_config<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let content = toml::to_string_pretty(value)
        .with_context(|| format!("serialize config {}", path.display()))?;
    save_atomic(path, &content)
}

fn save_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config path {} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!(".{file_name}.musman-tmp"));
    fs::write(&tmp, content).with_context(|| format!("write temp config {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace config {}", path.display()))?;
    Ok(())
}

fn validate_link(saved: &SavedLink) -> Result<()> {
    if saved.formats.is_empty() {
        bail!("link config has no formats; add at least one to `formats`");
    }
    if saved.formats_root.is_empty() {
        bail!("link config has an empty `formats_root`");
    }
    if saved.max_qual_root.is_empty() {
        bail!("link config has an empty `max_qual_root`");
    }
    Ok(())
}

pub fn dedup_formats(formats: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    formats
        .into_iter()
        .filter(|f| seen.insert(f.clone()))
        .collect()
}

pub fn parse_selection(raw: &str) -> Result<SyncSelection> {
    if raw == "best" {
        Ok(SyncSelection::Best)
    } else if let Some(ext) = raw.strip_prefix("only:") {
        if ext.is_empty() {
            bail!("invalid selection \"only:\"; expected `best` or `only:<ext>`");
        }
        Ok(SyncSelection::Format(ext.to_string()))
    } else {
        bail!("invalid selection \"{raw}\" in sync config; expected `best` or `only:<ext>`")
    }
}

pub fn selection_to_string(selection: &SyncSelection) -> String {
    match selection {
        SyncSelection::Best => "best".to_string(),
        SyncSelection::Format(ext) => format!("only:{ext}"),
    }
}

/// Compute `target` relative to `base`, using `..` when necessary.
pub fn relative_path(base: &Path, target: &Path) -> PathBuf {
    if base == target {
        return PathBuf::from(".");
    }
    if let Ok(stripped) = target.strip_prefix(base) {
        if stripped.as_os_str().is_empty() {
            return PathBuf::from(".");
        }
        return stripped.to_path_buf();
    }

    let base_components: Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let mut common = 0;
    for (b, t) in base_components.iter().zip(target_components.iter()) {
        if b == t {
            common += 1;
        } else {
            break;
        }
    }

    let mut out = PathBuf::new();
    for _ in common..base_components.len() {
        out.push("..");
    }
    for component in &target_components[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Ask a `[y/N]` question.
///
/// `assume_yes` answers yes without prompting. On a non-terminal stdin the
/// answer is always no (with a warning), so unattended runs never block and
/// never silently overwrite a saved config.
pub fn confirm(prompt: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("musman: warning: not a terminal; assuming no: {prompt}");
        return false;
    }
    print!("{prompt} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_config_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let saved = SavedLink {
            formats: vec!["flac".into(), "mp3".into()],
            formats_root: "formats".into(),
            max_qual_root: "max_qual".into(),
        };
        save_link(&source, &saved).unwrap();
        let loaded = load_link(&source).unwrap().unwrap();
        assert_eq!(loaded, saved);
    }

    #[test]
    fn sync_config_round_trip_and_default_no_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let saved = SavedSync {
            selection: "only:flac".into(),
            no_delete: true,
        };
        save_sync(&target, &saved).unwrap();
        let loaded = load_sync(&target).unwrap().unwrap();
        assert_eq!(loaded, saved);

        fs::write(target.join(CONFIG_FILE_NAME), "selection = \"best\"\n").unwrap();
        let loaded = load_sync(&target).unwrap().unwrap();
        assert_eq!(loaded.selection, "best");
        assert!(!loaded.no_delete);
    }

    #[test]
    fn missing_config_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_link(tmp.path()).unwrap().is_none());
        assert!(load_sync(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn malformed_link_config_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILE_NAME), "not toml").unwrap();
        assert!(load_link(tmp.path()).is_err());
    }

    #[test]
    fn invalid_saved_selection_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILE_NAME), "selection = \"bogus\"\n").unwrap();
        assert!(load_sync(tmp.path()).is_err());
    }

    #[test]
    fn parses_selections() {
        assert_eq!(parse_selection("best").unwrap(), SyncSelection::Best);
        assert_eq!(
            parse_selection("only:flac").unwrap(),
            SyncSelection::Format("flac".into())
        );
        assert!(parse_selection("only:").is_err());
        assert!(parse_selection("flac").is_err());
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative_path(Path::new("/a"), Path::new("/a")),
            PathBuf::from(".")
        );
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/b/c/d")),
            PathBuf::from("c/d")
        );
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/c/d")),
            PathBuf::from("../c/d")
        );
        assert_eq!(
            relative_path(Path::new("/a/b/c"), Path::new("/b")),
            PathBuf::from("../../../b")
        );
    }

    #[test]
    fn dedups_formats_preserving_order() {
        assert_eq!(
            dedup_formats(vec!["flac".into(), "mp3".into(), "flac".into()]),
            vec!["flac".to_string(), "mp3".to_string()]
        );
    }
}
