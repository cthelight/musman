//! Hard-link creation with the semantics of `ln -f src dest`.
//!
//! The source is never re-stat'ed here: its identity is captured once
//! during the scan and passed in. On a network mount every avoided
//! syscall is a round trip not taken.

use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

/// The identity of one file: its `(dev, ino)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeRef {
    /// Device number.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
}

impl InodeRef {
    /// Stat `path` (without following symlinks) and capture its identity.
    pub fn stat(path: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// True when `meta` describes the same file as this reference.
    pub fn matches(&self, meta: &fs::Metadata) -> bool {
        meta.dev() == self.dev && meta.ino() == self.ino
    }
}

/// The outcome of ensuring one hard link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    /// The destination already pointed at the same file; nothing was done.
    UpToDate,
    /// A new hard link was created.
    Created,
    /// The destination existed but pointed at a different file; it was
    /// removed and re-linked.
    Replaced,
}

/// Ensure `dest` is a hard link to `src`.
///
/// If `dest` is missing it is created (creating its parent directories
/// if needed); if it already points at `src` nothing happens; otherwise
/// (a stale link to a different inode, a regular file, or a symlink) it
/// is removed and re-linked. This mirrors the legacy `ln -f`.
///
/// `src_ino` is the identity of `src` captured at scan time. If `src`
/// was replaced between the scan and this call, the destination may be
/// left one run behind; the next reconciliation fixes it.
///
/// When `dry_run` is true nothing is written; the action that *would* be
/// taken is still returned so callers can report it.
pub fn ensure_hard_link(
    src: &Path,
    dest: &Path,
    src_ino: InodeRef,
    dry_run: bool,
    verbose: bool,
) -> Result<LinkAction> {
    let dest_meta = match fs::symlink_metadata(dest) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| format!("stat destination {}", dest.display()));
        }
    };

    match dest_meta {
        Some(meta) if meta.is_file() && src_ino.matches(&meta) => {
            if verbose {
                println!("ok      {}", dest.display());
            }
            Ok(LinkAction::UpToDate)
        }
        Some(_) => {
            if verbose {
                println!(
                    "{} {}",
                    if dry_run { "would replace" } else { "replace" },
                    dest.display()
                );
            }
            if !dry_run {
                fs::remove_file(dest)
                    .with_context(|| format!("remove stale link {}", dest.display()))?;
                do_link(src, dest)?;
            }
            Ok(LinkAction::Replaced)
        }
        None => {
            if verbose {
                println!(
                    "{} {}",
                    if dry_run { "would create" } else { "create" },
                    dest.display()
                );
            }
            if !dry_run {
                do_link(src, dest)?;
            }
            Ok(LinkAction::Created)
        }
    }
}

/// `link(2)`, creating `dest`'s parent directories if they do not exist
/// yet (first run).
///
/// A `NotFound` error means either the source vanished or some component
/// of `dest`'s parent chain is missing. Other link workers may be
/// creating the same parent concurrently, so `create_dir_all` (a no-op
/// when the chain already exists, tolerant of `EEXIST`) is called
/// unconditionally and the link is always retried: if the parent now
/// exists the retry succeeds, otherwise the error is the source having
/// vanished, which stays meaningful.
fn do_link(src: &Path, dest: &Path) -> Result<()> {
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create directory {}", parent.display()))?;
            }
            fs::hard_link(src, dest).with_context(|| link_context(src, dest))
        }
        Err(err) => Err(err).with_context(|| link_context(src, dest)),
    }
}

fn link_context(src: &Path, dest: &Path) -> String {
    format!(
        "hard link {} -> {} failed (hard links may not be supported on this filesystem)",
        dest.display(),
        src.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_of(path: &Path) -> InodeRef {
        InodeRef::stat(path).unwrap()
    }

    #[test]
    fn creates_a_hard_link() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("out/link.bin");
        fs::write(&src, "data").unwrap();

        let action = ensure_hard_link(&src, &dest, ref_of(&src), false, false).unwrap();
        assert_eq!(action, LinkAction::Created);
        assert_eq!(
            fs::metadata(&src).unwrap().ino(),
            fs::metadata(&dest).unwrap().ino()
        );
    }

    #[test]
    fn is_idempotent_when_already_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("link.bin");
        fs::write(&src, "data").unwrap();
        fs::hard_link(&src, &dest).unwrap();

        let action = ensure_hard_link(&src, &dest, ref_of(&src), false, false).unwrap();
        assert_eq!(action, LinkAction::UpToDate);
    }

    #[test]
    fn replaces_a_stale_link_to_a_different_inode() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let stale = tmp.path().join("stale.bin");
        let dest = tmp.path().join("link.bin");
        fs::write(&src, "new").unwrap();
        fs::write(&stale, "old").unwrap();
        fs::hard_link(&stale, &dest).unwrap();

        let action = ensure_hard_link(&src, &dest, ref_of(&src), false, false).unwrap();
        assert_eq!(action, LinkAction::Replaced);
        assert_eq!(
            fs::metadata(&src).unwrap().ino(),
            fs::metadata(&dest).unwrap().ino()
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("link.bin");
        fs::write(&src, "data").unwrap();

        let action = ensure_hard_link(&src, &dest, ref_of(&src), true, false).unwrap();
        assert_eq!(action, LinkAction::Created);
        assert!(!dest.exists());
    }

    #[test]
    fn replaces_a_symlink_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let target = tmp.path().join("target.bin");
        let dest = tmp.path().join("link.bin");
        fs::write(&src, "data").unwrap();
        fs::write(&target, "other").unwrap();
        std::os::unix::fs::symlink(&target, &dest).unwrap();

        let action = ensure_hard_link(&src, &dest, ref_of(&src), false, false).unwrap();
        assert_eq!(action, LinkAction::Replaced);
        // The symlink's target is untouched.
        assert!(target.exists());
        assert_eq!(
            fs::metadata(&src).unwrap().ino(),
            fs::metadata(&dest).unwrap().ino()
        );
    }
}
