//! Copying one source file to a destination with incremental semantics.
//!
//! A destination is up to date when it exists and its `(size, mtime)`
//! matches the source's. On copy we *preserve the source mtime* on the
//! destination, so the next run's comparison is stable: an unchanged file
//! is recognized as up to date without reading either file's contents, and
//! a re-encoded source (new size or mtime) is detected and re-copied. This
//! is what makes syncing to a slow removable device incremental — only the
//! files that actually changed are read from the source and written to the
//! device.
//!
//! Copies are atomic: contents go to a `<dest>.musman-tmp` file, whose mtime
//! is set to the source's, then it is renamed over `dest`. A crash mid-copy
//! leaves only the temp file behind; mirror-mode reconciliation removes it
//! (it is not part of the desired state), and `--no-delete` runs remove it
//! via [`remove_stale_temps`].

use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::walk;

/// Tolerance applied when comparing mtimes.
///
/// Coarse filesystems (FAT32, common on removable media) store mtime with
/// two-second granularity, so an exactly-preserved mtime can round-trip off
/// by up to a second. A two-second window absorbs that without masking a
/// real change.
pub const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

/// Suffix appended to a destination's name for its in-progress copy.
const TEMP_SUFFIX: &str = ".musman-tmp";

/// The outcome of ensuring one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAction {
    /// The destination already matched the source; nothing was done.
    UpToDate,
    /// The destination was missing or stale and was (re-)copied.
    Copied,
}

/// Ensure `dest` is a copy of `src` whose size and mtime match.
///
/// `size` and `mtime` are the source's, captured at scan time so the source
/// is not re-stat'ed here. When `dry_run` is true nothing is written; the
/// action that *would* be taken is still returned so callers can report it.
pub fn ensure_copied(
    src: &Path,
    dest: &Path,
    size: u64,
    mtime: SystemTime,
    dry_run: bool,
    verbose: bool,
) -> Result<CopyAction> {
    let dest_meta = match fs::symlink_metadata(dest) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| format!("stat destination {}", dest.display()));
        }
    };

    let up_to_date = dest_meta
        .is_some_and(|meta| meta.is_file() && meta.len() == size && mtime_within(&meta, mtime));

    if up_to_date {
        if verbose {
            println!("ok      {}", dest.display());
        }
        return Ok(CopyAction::UpToDate);
    }

    if verbose {
        println!(
            "{} {}",
            if dry_run { "would copy" } else { "copy" },
            dest.display()
        );
    }
    if !dry_run {
        do_copy(src, dest, mtime)?;
    }
    Ok(CopyAction::Copied)
}

/// True when `meta`'s mtime is within `MTIME_TOLERANCE` of `mtime`.
fn mtime_within(meta: &fs::Metadata, mtime: SystemTime) -> bool {
    let Ok(dest_mtime) = meta.modified() else {
        return false;
    };
    let diff = dest_mtime
        .duration_since(mtime)
        .unwrap_or_else(|_| mtime.duration_since(dest_mtime).unwrap_or(Duration::ZERO));
    diff <= MTIME_TOLERANCE
}

/// Atomically copy `src` to `dest`, preserving `mtime`.
///
/// A `NotFound` error means either the source vanished or some component of
/// `dest`'s parent chain is missing. Other copy workers may be creating the
/// same parent concurrently, so `create_dir_all` (a no-op when the chain
/// already exists, tolerant of `EEXIST`) is called and the copy is retried:
/// if the parent now exists the retry succeeds, otherwise the error is the
/// source having vanished, which stays meaningful.
fn do_copy(src: &Path, dest: &Path, mtime: SystemTime) -> Result<()> {
    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty());
    let temp = temp_path(dest);

    match write_and_move(src, &temp, dest, mtime) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(parent) = parent {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create directory {}", parent.display()))?;
            }
            write_and_move(src, &temp, dest, mtime).with_context(|| copy_context(src, dest))
        }
        Err(err) => Err(err).with_context(|| copy_context(src, dest)),
    }
}

/// Write `src` to `temp`, set its mtime, and rename it over `dest`. The temp
/// file is removed if anything fails, so a failed copy leaves no litter.
///
/// Returns the raw I/O error so the caller can retry when a `NotFound` is
/// caused by a missing parent directory rather than a vanished source.
fn write_and_move(src: &Path, temp: &Path, dest: &Path, mtime: SystemTime) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        write_temp(src, temp, mtime)?;
        fs::rename(temp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Stream `src` into `temp`, then set `temp`'s mtime to the source's.
fn write_temp(src: &Path, temp: &Path, mtime: SystemTime) -> io::Result<()> {
    let mut out = File::create(temp)?;
    let mut input = File::open(src)?;
    io::copy(&mut input, &mut out)?;
    out.flush()?;
    // Preserve the source mtime so the next run's (size, mtime) comparison
    // sees this file as up to date.
    out.set_times(FileTimes::new().set_modified(mtime))?;
    Ok(())
}

/// The in-progress name for `dest`: its file name plus `TEMP_SUFFIX`.
fn temp_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .expect("destination has a file name")
        .to_os_string();
    name.push(OsString::from(TEMP_SUFFIX));
    dest.with_file_name(name)
}

fn copy_context(src: &Path, dest: &Path) -> String {
    format!("copy {} -> {} failed", src.display(), dest.display())
}

/// Remove the in-progress temp files a crashed run left under `root`.
///
/// Mirror-mode reconciliation already removes them (they are not part of
/// the desired state); this is what cleans them on `--no-delete` runs,
/// where nothing else walks the target. Only regular files whose name ends
/// in `TEMP_SUFFIX` are removed; everything else is untouched.
pub fn remove_stale_temps(root: &Path, dry_run: bool, verbose: bool) -> Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let stale: Vec<PathBuf> = walk::walk_all(root)
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let is_temp = entry.path.file_name().is_some_and(|name| {
                name.to_str()
                    .is_some_and(|name| name.ends_with(TEMP_SUFFIX))
            });
            (entry.kind == walk::Kind::File && is_temp).then_some(entry.path)
        })
        .collect();
    let results: Vec<Result<()>> = stale
        .par_iter()
        .map(|path| {
            if verbose {
                println!(
                    "{}  {}",
                    if dry_run { "would remove" } else { "remove" },
                    path.display()
                );
            }
            if !dry_run {
                fs::remove_file(path)
                    .with_context(|| format!("remove stale temp file {}", path.display()))?;
            }
            Ok(())
        })
        .collect();
    let mut removed = 0;
    for result in results {
        result?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::FileId;
    use std::os::unix::fs::MetadataExt;

    fn set_mtime(path: &Path, mtime: SystemTime) {
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();
    }

    fn inode(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }

    #[test]
    fn copies_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("out/dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();

        let action = ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert_eq!(action, CopyAction::Copied);
        assert_eq!(fs::read(&dest).unwrap(), b"data");
        assert!(mtime_within(&fs::metadata(&dest).unwrap(), id.mtime));
    }

    #[test]
    fn is_up_to_date_when_size_and_mtime_match() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();

        ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        let ino_before = inode(&dest);

        let id = FileId::stat(&src).unwrap();
        let action = ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert_eq!(action, CopyAction::UpToDate);
        assert_eq!(
            ino_before,
            inode(&dest),
            "an up-to-date file must not be rewritten"
        );
    }

    #[test]
    fn recopies_when_size_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();
        ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();

        fs::write(&src, "much longer data").unwrap();
        let id = FileId::stat(&src).unwrap();
        let action = ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert_eq!(action, CopyAction::Copied);
        assert_eq!(fs::read(&dest).unwrap(), b"much longer data");
    }

    #[test]
    fn recopies_when_mtime_moves_beyond_tolerance() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();
        ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();

        // Same size, but the source mtime jumps far outside the tolerance.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        set_mtime(&src, old);
        let id = FileId::stat(&src).unwrap();
        let action = ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert_eq!(action, CopyAction::Copied);
        assert!(mtime_within(&fs::metadata(&dest).unwrap(), old));
    }

    #[test]
    fn preserves_source_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        set_mtime(&src, target);
        let id = FileId::stat(&src).unwrap();

        ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert!(
            mtime_within(&fs::metadata(&dest).unwrap(), target),
            "the destination mtime must track the source mtime"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();

        let action = ensure_copied(&src, &dest, id.size, id.mtime, true, false).unwrap();
        assert_eq!(action, CopyAction::Copied);
        assert!(!dest.exists());
        assert!(!temp_path(&dest).exists());
    }

    #[test]
    fn creates_parent_directories_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("a/b/c/dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();

        let action = ensure_copied(&src, &dest, id.size, id.mtime, false, false).unwrap();
        assert_eq!(action, CopyAction::Copied);
        assert_eq!(fs::read(&dest).unwrap(), b"data");
    }

    #[test]
    fn a_failed_copy_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, "data").unwrap();
        let id = FileId::stat(&src).unwrap();

        // Make the destination an existing directory so the final rename
        // fails; the in-progress temp file must be cleaned up.
        fs::create_dir_all(&dest).unwrap();
        let err = ensure_copied(&src, &dest, id.size, id.mtime, false, false);
        assert!(err.is_err(), "copying over a directory must fail");
        assert!(
            !temp_path(&dest).exists(),
            "a failed copy must not leave a temp file"
        );
    }
}
