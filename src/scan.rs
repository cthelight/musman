//! Single-pass scan of the source tree.
//!
//! One parallel walk replaces the legacy script's per-format `find`
//! passes and per-file process spawns: every regular file is visited
//! exactly once, and files are grouped into tracks in memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rayon::prelude::*;

use crate::link::InodeRef;
use crate::names::{split_name, track_key};
use crate::walk::{self, Kind};

/// A source file whose extension is one of the managed formats.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Absolute path of the file in the source tree.
    pub path: PathBuf,
    /// Path relative to the source root.
    pub rel: PathBuf,
    /// The file's extension, e.g. `"flac"`.
    pub ext: String,
    /// Track key (relative path minus the final extension).
    pub track: String,
    /// Identity of the file, captured here so the link phase does not
    /// have to stat the source again (a round trip on network mounts).
    pub ino: InodeRef,
}

/// One track: all source files that share a track key, plus the
/// best-quality one of them.
#[derive(Debug, Clone)]
pub struct Track {
    /// Track key.
    pub key: String,
    /// Extension of this track's best-quality source file.
    pub best_ext: String,
    /// Absolute path of the best-quality source file.
    pub best_path: PathBuf,
    /// Identity of the best-quality source file.
    pub best_ino: InodeRef,
    /// Position of `best_ext` in the format order (lower is better).
    best_index: usize,
}

/// Result of scanning the source tree.
#[derive(Debug, Default)]
pub struct SourceScan {
    /// Regular files seen in the source tree (managed or not), excluding
    /// the pruned output subtrees.
    pub regular_files: usize,
    /// Managed files (extension in the format set).
    pub files: Vec<SourceFile>,
    /// One entry per track key, holding the best available format.
    pub tracks: HashMap<String, Track>,
    /// Non-fatal problems (e.g. unreadable subtrees), formatted for display.
    pub errors: Vec<String>,
}

/// Classification of one walked entry during the scan.
enum EntryOutcome {
    /// Excluded (inside an output root) or not a regular file.
    Ignored,
    /// A regular file whose extension is not managed.
    Other,
    /// A managed source file.
    File(SourceFile),
}

/// Walk `source` and collect all managed files plus the best format per
/// track.
///
/// `excluded` are canonical paths (the output roots) that are pruned from
/// the walk even when they are nested inside the source tree, so the
/// tool never links its own output trees.
///
/// `format_index` maps each managed format to its quality position
/// (0 = best).
pub fn scan(
    source: &Path,
    excluded: &[PathBuf],
    format_index: &HashMap<String, usize>,
) -> Result<SourceScan> {
    let entries = walk::walk_all(source);

    let processed: Vec<Result<EntryOutcome>> = entries
        .into_par_iter()
        .map(|entry| {
            let entry = entry?;
            if excluded
                .iter()
                .any(|ex| entry.path == *ex || entry.path.starts_with(ex.as_path()))
            {
                return Ok(EntryOutcome::Ignored);
            }
            if entry.kind != Kind::File {
                return Ok(EntryOutcome::Ignored);
            }

            let name = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow!("internal: non-UTF-8 file name in source tree"))?;
            let Some((stem, ext)) = split_name(name) else {
                return Ok(EntryOutcome::Other);
            };
            if !format_index.contains_key(ext) {
                return Ok(EntryOutcome::Other);
            }

            let rel = entry.path.strip_prefix(source).map_err(|_| {
                anyhow!(
                    "internal: walked path {:?} is not under the source root",
                    entry.path
                )
            })?;

            // Capture the file's identity now: the link phase must not
            // stat the source again (one round trip per file on a slow
            // NFS mount).
            let ino = InodeRef::stat(&entry.path)
                .with_context(|| format!("stat source file {}", entry.path.display()))?;

            Ok(EntryOutcome::File(SourceFile {
                rel: rel.to_path_buf(),
                ext: ext.to_string(),
                track: track_key(rel.parent().unwrap_or(Path::new("")), stem),
                path: entry.path,
                ino,
            }))
        })
        .collect();

    let mut regular_files = 0usize;
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for result in processed {
        match result {
            Ok(EntryOutcome::File(file)) => {
                regular_files += 1;
                files.push(file);
            }
            Ok(EntryOutcome::Other) => regular_files += 1,
            Ok(EntryOutcome::Ignored) => {}
            Err(err) => errors.push(format!("{err:#}")),
        }
    }

    let mut tracks: HashMap<String, Track> = HashMap::new();
    for file in &files {
        let index = format_index
            .get(file.ext.as_str())
            .copied()
            .expect("managed file has an indexed extension");
        match tracks.get_mut(&file.track) {
            Some(track) if track.best_index <= index => {}
            Some(track) => {
                track.best_index = index;
                track.best_ext = file.ext.clone();
                track.best_path = file.path.clone();
                track.best_ino = file.ino;
            }
            None => {
                tracks.insert(
                    file.track.clone(),
                    Track {
                        key: file.track.clone(),
                        best_ext: file.ext.clone(),
                        best_path: file.path.clone(),
                        best_ino: file.ino,
                        best_index: index,
                    },
                );
            }
        }
    }

    Ok(SourceScan {
        regular_files,
        files,
        tracks,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;

    fn index(formats: &[&str]) -> HashMap<String, usize> {
        formats
            .iter()
            .enumerate()
            .map(|(i, f)| (f.to_string(), i))
            .collect()
    }

    #[test]
    fn groups_files_into_tracks_by_best_format() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        fs::create_dir_all(src.join("album")).unwrap();
        fs::write(src.join("album/01 Song.flac"), "a").unwrap();
        fs::write(src.join("album/01 Song.mp3"), "b").unwrap();
        fs::write(src.join("album/02 Song.mp3"), "c").unwrap();
        fs::write(src.join("album/03 Song.ogg"), "d").unwrap();
        fs::write(src.join("unmanaged.txt"), "e").unwrap();

        let scan = scan(src, &[], &index(&["flac", "mp3", "ogg"])).unwrap();

        assert_eq!(scan.regular_files, 5);
        assert_eq!(scan.files.len(), 4);
        assert_eq!(scan.tracks.len(), 3);

        let t01 = scan.tracks.get("album/01 Song").unwrap();
        assert_eq!(t01.best_ext, "flac");
        assert!(t01.best_path.ends_with("album/01 Song.flac"));

        let t02 = scan.tracks.get("album/02 Song").unwrap();
        assert_eq!(t02.best_ext, "mp3");

        let t03 = scan.tracks.get("album/03 Song").unwrap();
        assert_eq!(t03.best_ext, "ogg");
    }

    #[test]
    fn excluded_subtrees_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        fs::create_dir_all(src.join("formats/flac")).unwrap();
        fs::create_dir_all(src.join("max_qual")).unwrap();
        fs::write(src.join("a.flac"), "x").unwrap();
        fs::write(src.join("formats/flac/a.flac"), "y").unwrap();
        fs::write(src.join("max_qual/a.flac"), "z").unwrap();

        let excluded = vec![
            src.join("formats").to_path_buf(),
            src.join("max_qual").to_path_buf(),
        ];
        let scan = scan(src, &excluded, &index(&["flac"])).unwrap();

        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("a.flac"));
        assert_eq!(scan.files[0].rel.to_str().unwrap(), "a.flac");
    }

    #[test]
    fn hidden_files_and_dotfiles_are_scanned_like_find() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        fs::create_dir_all(src.join(".hidden dir")).unwrap();
        fs::write(src.join(".hidden dir/secret.flac"), "x").unwrap();

        let scan = scan(src, &[], &index(&["flac"])).unwrap();
        assert_eq!(scan.files.len(), 1);
    }

    #[test]
    fn non_utf8_names_do_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        let odd = src.join(std::ffi::OsString::from_vec(vec![
            b's', 0xff, b'l', b'a', b'c',
        ]));
        fs::write(&odd, "x").unwrap();

        let scan = scan(src, &[], &index(&["flac"]));
        // The walk itself must succeed; the odd file is reported, not fatal.
        assert!(scan.is_ok());
    }
}
