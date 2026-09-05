//! Reconciling one output tree with the desired state.
//!
//! The tree is walked exactly once: while reading each directory (one
//! `readdir` per directory, classification via `d_type`), regular files
//! the desired state does not want are collected, then removed in
//! parallel, then directories left empty are pruned bottom-up from the
//! same read — no second walk, no `read_dir` twice, no per-file `stat`.
//! On a slow NFS mount that is the difference between seconds and
//! minutes.
//!
//! Directories named `.stfolder` (syncthing markers) are never walked
//! and never removed: syncthing keeps its own metadata there and it must
//! not be touched.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;

/// Classification of one directory from the reconciliation pass.
struct DirScan {
    /// The directory itself.
    path: PathBuf,
    /// Entries that will still exist after this run: kept files,
    /// symlinks and other special entries, and protected subdirectories.
    kept_entries: usize,
    /// Subdirectories reconciled in this pass (children of `path`).
    subdirs: Vec<PathBuf>,
    /// Regular files to remove.
    remove_files: Vec<PathBuf>,
}

/// Reconcile the tree rooted at `root` with the desired state.
///
/// Every regular file under `root` that is not in `keep` is removed
/// (symlinks and other special files are left alone), then directories
/// left empty are removed bottom-up — including directories that were
/// already empty before this run. `root` itself is never removed, and
/// `.stfolder` directories and their contents are never touched.
///
/// Returns the files removed (or that would be removed in a dry run) and
/// the number of directories removed (or to be removed).
pub fn reconcile_tree(
    root: &Path,
    keep: &HashSet<PathBuf>,
    dry_run: bool,
    verbose: bool,
) -> Result<(Vec<PathBuf>, usize)> {
    if !root.is_dir() {
        return Ok((Vec::new(), 0));
    }

    // Pass 1: one readdir per directory, in parallel.
    let mut scans: Vec<DirScan> = Vec::new();
    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    while !level.is_empty() {
        let results: Vec<Result<DirScan>> =
            level.par_iter().map(|dir| scan_dir(dir, keep)).collect();
        let mut next = Vec::new();
        for scan in results {
            let scan = scan?;
            // Clone (not drain): the scans keep their subdirs for the
            // bottom-up prune pass.
            next.extend(scan.subdirs.iter().cloned());
            scans.push(scan);
        }
        level = next;
    }

    // Pass 2: remove the collected files, in parallel.
    let remove_files: Vec<PathBuf> = scans
        .iter()
        .flat_map(|s| s.remove_files.iter().cloned())
        .collect();
    let removed_files: Vec<PathBuf> = if remove_files.is_empty() {
        Vec::new()
    } else {
        let results: Vec<Result<PathBuf>> = remove_files
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
                        .with_context(|| format!("remove unwanted file {}", path.display()))?;
                }
                Ok(path.clone())
            })
            .collect();
        results.into_iter().collect::<Result<Vec<_>>>()?
    };

    // Pass 3: prune directories bottom-up. Children always have more path
    // components than their parent, so this order is safe.
    let mut order: Vec<&DirScan> = scans.iter().collect();
    order.sort_by_key(|d| std::cmp::Reverse(d.path.components().count()));

    let mut gone: HashSet<PathBuf> = HashSet::new();
    let mut removed_dirs = 0;
    for scan in order {
        if scan.path == *root {
            continue;
        }
        let empty = scan.kept_entries == 0 && scan.subdirs.iter().all(|s| gone.contains(s));
        if !empty {
            continue;
        }
        if dry_run {
            // Simulate, so parents can be pruned in the same pass.
            if verbose {
                println!("would prune  {}", scan.path.display());
            }
            removed_dirs += 1;
            gone.insert(scan.path.clone());
            continue;
        }
        match fs::remove_dir(&scan.path) {
            Ok(()) => {
                if verbose {
                    println!("prune  {}", scan.path.display());
                }
                removed_dirs += 1;
                gone.insert(scan.path.clone());
            }
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) =>
            {
                // Concurrent change (e.g. syncthing); leave it for the
                // next run.
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("remove empty directory {}", scan.path.display()));
            }
        }
    }

    Ok((removed_files, removed_dirs))
}

/// Read one directory and classify its entries.
fn scan_dir(dir: &Path, keep: &HashSet<PathBuf>) -> Result<DirScan> {
    let mut scan = DirScan {
        path: dir.to_path_buf(),
        kept_entries: 0,
        subdirs: Vec::new(),
        remove_files: Vec::new(),
    };

    let read_dir =
        fs::read_dir(dir).with_context(|| format!("read directory {}", dir.display()))?;
    for entry in read_dir {
        let entry = entry.with_context(|| format!("read directory entry in {}", dir.display()))?;
        let path = entry.path();
        // d_type from the readdir result when available.
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_dir() {
            if path.file_name().is_some_and(|n| n == ".stfolder") {
                // Syncthing marker: not touched, and its contents are
                // syncthing's own metadata, not ours.
                scan.kept_entries += 1;
            } else {
                scan.subdirs.push(path);
            }
        } else if file_type.is_file() && !keep.contains(&path) {
            scan.remove_files.push(path);
        } else {
            scan.kept_entries += 1;
        }
    }
    Ok(scan)
}

/// Ensure the syncthing marker directory exists under `max_qual_root`.
///
/// Syncthing drops a folder that becomes completely empty; keeping a
/// marker directory guarantees the folder is always considered
/// non-empty. When the marker already exists it is left completely
/// untouched (no recreation, no mtime churn).
pub fn ensure_stfolder(max_qual_root: &Path, dry_run: bool, verbose: bool) -> Result<()> {
    let marker = max_qual_root.join(".stfolder");
    if marker.is_dir() {
        return Ok(());
    }
    if verbose {
        println!(
            "{}  {}",
            if dry_run { "would create" } else { "create" },
            marker.display()
        );
    }
    if !dry_run {
        fs::create_dir_all(&marker)
            .with_context(|| format!("create syncthing marker {}", marker.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn removes_only_files_not_in_keep_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        let keep_me = root.join("keep.bin");
        let drop_me = root.join("drop.bin");
        let drop_nested = root.join("sub/nested.bin");
        fs::write(&keep_me, "k").unwrap();
        fs::write(&drop_me, "d").unwrap();
        fs::write(&drop_nested, "n").unwrap();

        let mut keep = HashSet::new();
        keep.insert(keep_me.clone());
        let (removed, pruned) = reconcile_tree(root, &keep, false, false).unwrap();

        assert_eq!(removed.len(), 2);
        assert!(keep_me.exists());
        assert!(!drop_me.exists());
        assert!(!drop_nested.exists());
        // sub became empty and was pruned.
        assert_eq!(pruned, 1);
        assert!(!root.join("sub").exists());
    }

    #[test]
    fn leaves_symlinks_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = outside.path().join("target.bin");
        let link = root.join("link.bin");
        fs::write(&target, "t").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (removed, pruned) = reconcile_tree(root, &HashSet::new(), false, false).unwrap();
        assert!(removed.is_empty());
        assert_eq!(pruned, 0);
        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert!(target.exists());
    }

    #[test]
    fn dry_run_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.bin"), "a").unwrap();
        let (removed, pruned) = reconcile_tree(root, &HashSet::new(), true, false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(pruned, 0);
        assert!(root.join("a.bin").exists());
    }

    #[test]
    fn stfolder_and_its_contents_are_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stfolder = root.join(".stfolder");
        fs::create_dir_all(&stfolder).unwrap();
        fs::write(stfolder.join("metadata.bin"), "m").unwrap();
        fs::write(root.join("stale.bin"), "s").unwrap();

        let (removed, pruned) = reconcile_tree(root, &HashSet::new(), false, false).unwrap();

        assert_eq!(removed.len(), 1);
        // The .stfolder counts as a kept entry, so nothing is pruned.
        assert_eq!(pruned, 0);
        assert!(stfolder.is_dir());
        assert!(stfolder.join("metadata.bin").exists());
    }

    #[test]
    fn cleans_subtrees_of_unmanaged_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("ogg")).unwrap();
        fs::create_dir_all(root.join("flac")).unwrap();
        let stale_ogg = root.join("ogg/x.ogg");
        let keep_flac = root.join("flac/y.flac");
        fs::write(&stale_ogg, "o").unwrap();
        fs::write(&keep_flac, "f").unwrap();

        let mut keep = HashSet::new();
        keep.insert(keep_flac.clone());
        let (removed, pruned) = reconcile_tree(root, &keep, false, false).unwrap();

        assert_eq!(removed, vec![stale_ogg]);
        assert_eq!(pruned, 1);
        assert!(!root.join("ogg").exists());
        assert!(keep_flac.exists());
    }

    #[test]
    fn keeps_intermediate_directories_with_kept_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        let kept = root.join("a/b/kept.bin");
        fs::write(&kept, "k").unwrap();

        let mut keep = HashSet::new();
        keep.insert(kept.clone());
        let (removed, pruned) = reconcile_tree(root, &keep, false, false).unwrap();

        // Nothing is stale, and the intermediate directories have
        // non-empty descendants, so nothing may be pruned.
        assert!(removed.is_empty());
        assert_eq!(pruned, 0);
        assert!(kept.exists());
        assert!(root.join("a").is_dir());
        assert!(root.join("a/b").is_dir());
    }

    #[test]
    fn prunes_only_the_empty_branch_of_a_mixed_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("a/c")).unwrap();
        let kept = root.join("a/b/kept.bin");
        let stale = root.join("a/c/stale.bin");
        fs::write(&kept, "k").unwrap();
        fs::write(&stale, "s").unwrap();

        let mut keep = HashSet::new();
        keep.insert(kept.clone());
        let (removed, pruned) = reconcile_tree(root, &keep, false, false).unwrap();

        // Only `c` becomes empty; `a` (mixed) and `b` (kept) survive.
        assert_eq!(removed, vec![stale]);
        assert_eq!(pruned, 1);
        assert!(kept.exists());
        assert!(root.join("a").is_dir());
        assert!(root.join("a/b").is_dir());
        assert!(!root.join("a/c").exists());
    }

    #[test]
    fn prunes_directories_that_were_already_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();

        let (removed, pruned) = reconcile_tree(root, &HashSet::new(), false, false).unwrap();
        assert!(removed.is_empty());
        assert_eq!(pruned, 2);
        assert!(!root.join("a").exists());
    }
}
