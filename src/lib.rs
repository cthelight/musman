//! musman: fast hard-link manager for multi-format music libraries.
//!
//! musman keeps two derived trees in sync with a source music library by
//! maintaining hard links:
//!
//! - `formats_root/<ext>/<relative path>`: one link per source file,
//!   grouped by format.
//! - `max_qual_root/<relative path>.<best ext>`: one link per *track*
//!   (relative path minus its final extension), pointing at the best
//!   available format according to the configured quality order.
//!
//! The tool is a *reconciler*: it computes the desired end state from a
//! single pass over the source tree, then creates missing links,
//! re-points stale ones, removes links the desired state does not want,
//! and prunes empty directories. The source tree is never modified.

mod clean;
mod copy;
mod link;
mod names;
mod scan;
mod stats;
mod walk;

pub use stats::Stats;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;

/// Resolved, validated configuration for one run.
#[derive(Debug, Clone)]
pub struct Config {
    /// Canonical source directory (the music library, never modified).
    pub source: PathBuf,
    /// Canonical root for the per-format link trees. For `sync` this path
    /// is only used to exclude the derived tree from the source scan.
    pub formats_root: PathBuf,
    /// Canonical root for the best-quality link tree. For `sync` this path
    /// is only used to exclude the derived tree from the source scan.
    pub max_qual_root: PathBuf,
    /// Managed formats, best first (duplicates removed).
    pub format_order: Vec<String>,
    /// Print one line per file action.
    pub verbose: bool,
    /// Report planned actions without touching the disk.
    pub dry_run: bool,
}

impl Config {
    /// Validate and canonicalize a configuration.
    ///
    /// Refuses layouts that could damage the library: the source
    /// directory must not coincide with, or be nested inside, an output
    /// directory, and the two output directories must not overlap.
    pub fn new(
        source: PathBuf,
        formats_root: PathBuf,
        max_qual_root: PathBuf,
        format_order: Vec<String>,
        verbose: bool,
        dry_run: bool,
    ) -> Result<Self> {
        if format_order.is_empty() {
            bail!("at least one format is required (use -f)");
        }
        let mut seen = HashSet::new();
        let format_order = format_order
            .into_iter()
            .filter(|f| seen.insert(f.clone()))
            .collect();

        let source = fs::canonicalize(&source)
            .with_context(|| format!("resolving source directory {}", source.display()))?;
        if !source.is_dir() {
            bail!("source {} is not a directory", source.display());
        }
        let formats_root = resolve_existing(&formats_root)?;
        let max_qual_root = resolve_existing(&max_qual_root)?;

        check_layout(&source, &formats_root, &max_qual_root)?;

        Ok(Self {
            source,
            formats_root,
            max_qual_root,
            format_order,
            verbose,
            dry_run,
        })
    }
}

fn check_layout(source: &Path, formats_root: &Path, max_qual_root: &Path) -> Result<()> {
    if source == formats_root || source == max_qual_root {
        bail!(
            "the source directory ({}) is the same as an output directory; refusing to run",
            source.display()
        );
    }
    if is_under(source, formats_root) || is_under(source, max_qual_root) {
        bail!(
            "the source directory ({}) is inside an output directory; refusing to run",
            source.display()
        );
    }
    if formats_root == max_qual_root {
        bail!("the formats and max-qual output directories must differ");
    }
    if is_under(formats_root, max_qual_root) || is_under(max_qual_root, formats_root) {
        bail!("the output directories must not be nested inside each other");
    }
    Ok(())
}

fn is_under(path: &Path, ancestor: &Path) -> bool {
    path != ancestor && path.starts_with(ancestor)
}

/// Canonicalize a path whose final components may not exist yet, by
/// canonicalizing the deepest existing ancestor and re-joining the rest.
fn resolve_existing(p: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = p.to_path_buf();
    while !cur.exists() {
        let name = cur
            .file_name()
            .ok_or_else(|| anyhow!("cannot resolve path {}", p.display()))?
            .to_os_string();
        missing.push(name);
        match cur.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cur = parent.to_path_buf(),
            _ => {
                cur = PathBuf::from(".");
            }
        }
    }
    let base = fs::canonicalize(&cur)
        .with_context(|| format!("resolving output directory {}", p.display()))?;
    let mut out = base;
    for name in missing.iter().rev() {
        out = out.join(name);
    }
    Ok(out)
}

/// Run one reconciliation.
pub fn run(cfg: &Config) -> Result<Stats> {
    let mut stats = Stats::default();

    let format_index: HashMap<String, usize> = cfg
        .format_order
        .iter()
        .enumerate()
        .map(|(i, f)| (f.clone(), i))
        .collect();

    // 1. Scan the source tree once, pruning the output roots so we never
    //    link our own output.
    let excluded = vec![cfg.formats_root.clone(), cfg.max_qual_root.clone()];
    let scan = scan::scan(&cfg.source, &excluded, &format_index)?;
    stats.regular_files = scan.regular_files;
    stats.managed_files = scan.files.len();
    stats.tracks = scan.tracks.len();
    stats.errors.extend(scan.errors);

    // 2. Compute the desired links.
    let mut format_links: Vec<(PathBuf, PathBuf, link::FileId)> =
        Vec::with_capacity(scan.files.len());
    for f in &scan.files {
        let dest = cfg.formats_root.join(&f.ext).join(&f.rel);
        format_links.push((f.path.clone(), dest, f.id));
    }
    let mut track_links: Vec<(PathBuf, PathBuf, link::FileId)> =
        Vec::with_capacity(scan.tracks.len());
    for t in scan.tracks.values() {
        let dest = cfg.max_qual_root.join(format!("{}.{}", t.key, t.best_ext));
        track_links.push((t.best_path.clone(), dest, t.best_id));
    }

    // 3. Create the output roots (never in a dry run). Parent
    //    directories of individual links are created on demand by the
    //    link itself, so steady-state runs cost no directory stats.
    if !cfg.dry_run {
        fs::create_dir_all(&cfg.formats_root)
            .with_context(|| format!("create directory {}", cfg.formats_root.display()))?;
        fs::create_dir_all(&cfg.max_qual_root)
            .with_context(|| format!("create directory {}", cfg.max_qual_root.display()))?;
    }

    // 4. Ensure every desired hard link (in parallel).
    let all_links: Vec<(PathBuf, PathBuf, link::FileId)> = format_links
        .iter()
        .chain(track_links.iter())
        .cloned()
        .collect();
    let link_results: Vec<Result<link::LinkAction>> = all_links
        .par_iter()
        .map(|(src, dest, id)| link::ensure_hard_link(src, dest, *id, cfg.dry_run, cfg.verbose))
        .collect();
    for result in link_results {
        match result {
            Ok(link::LinkAction::UpToDate) => stats.links_up_to_date += 1,
            Ok(link::LinkAction::Created) => stats.links_created += 1,
            Ok(link::LinkAction::Replaced) => stats.links_replaced += 1,
            Err(err) => stats.errors.push(format!("{err:#}")),
        }
    }

    // 5. Remove everything the desired state does not want.
    //
    // Both output trees are pure derived artifacts: every regular file
    // that is not a desired link is removed (including subdirectories of
    // formats that are no longer managed), and directories left empty
    // are pruned — in a single pass per tree. So the trees always match
    // the source tree exactly.
    let format_keep: HashSet<PathBuf> = format_links.iter().map(|(_, d, _)| d.clone()).collect();
    let track_keep: HashSet<PathBuf> = track_links.iter().map(|(_, d, _)| d.clone()).collect();

    let (format_removed, format_dirs) =
        clean::reconcile_tree(&cfg.formats_root, &format_keep, cfg.dry_run, cfg.verbose)?;
    stats.files_removed += format_removed.len();
    stats.dirs_removed += format_dirs;
    let (track_removed, track_dirs) =
        clean::reconcile_tree(&cfg.max_qual_root, &track_keep, cfg.dry_run, cfg.verbose)?;
    stats.files_removed += track_removed.len();
    stats.dirs_removed += track_dirs;

    Ok(stats)
}

/// What a `sync` run materializes on the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncSelection {
    /// All source files of one managed format, at their source relative paths.
    Format(String),
    /// The best-quality file of each track (per the format order).
    Best,
}

/// Run one incremental sync of the selected view to `target`.
///
/// Copies only the files that are missing or stale (by size + mtime),
/// preserves each source mtime on the destination, and — unless
/// `no_delete` is set — removes everything the selected view does not want,
/// so the destination becomes an exact mirror of it.
pub fn run_sync(
    cfg: &Config,
    target: &Path,
    selection: &SyncSelection,
    no_delete: bool,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let format_index: HashMap<String, usize> = cfg
        .format_order
        .iter()
        .enumerate()
        .map(|(i, f)| (f.clone(), i))
        .collect();

    // 1. Scan the source tree once, pruning the link output roots so we
    //    never copy our own derived trees.
    let excluded = vec![cfg.formats_root.clone(), cfg.max_qual_root.clone()];
    // Warn (but do not refuse) when the source appears to hold a derived
    // tree the exclusion roots do not cover: its files would otherwise be
    // copied as if they were source content.
    for excluded_root in &excluded {
        if let Some(name) = excluded_root.file_name() {
            let in_source = cfg.source.join(name);
            if !is_under(excluded_root, &cfg.source) && in_source.is_dir() {
                eprintln!(
                    "musman: warning: {} is inside the source but not excluded by -i/-m; its files are treated as source content",
                    in_source.display()
                );
            }
        }
    }
    let scan = scan::scan(&cfg.source, &excluded, &format_index)?;
    stats.regular_files = scan.regular_files;
    stats.managed_files = scan.files.len();
    stats.tracks = scan.tracks.len();
    stats.errors.extend(scan.errors);

    // 2. Compute the desired copies: (source, destination, size, mtime).
    let mut pairs: Vec<(PathBuf, PathBuf, u64, SystemTime)> = Vec::new();
    match selection {
        SyncSelection::Format(ext) => {
            pairs.reserve(scan.files.len());
            for f in &scan.files {
                if f.ext == *ext {
                    let dest = target.join(&f.rel);
                    pairs.push((f.path.clone(), dest, f.id.size, f.id.mtime));
                }
            }
        }
        SyncSelection::Best => {
            pairs.reserve(scan.tracks.len());
            for t in scan.tracks.values() {
                let dest = target.join(format!("{}.{}", t.key, t.best_ext));
                pairs.push((t.best_path.clone(), dest, t.best_id.size, t.best_id.mtime));
            }
        }
    }

    // 3. Create the destination root (never in a dry run). Parent
    //    directories of individual copies are created on demand.
    if !cfg.dry_run {
        fs::create_dir_all(target)
            .with_context(|| format!("create directory {}", target.display()))?;
    }

    // 4. Copy every desired file that is missing or stale (in parallel).
    let copy_results: Vec<Result<copy::CopyAction>> = pairs
        .par_iter()
        .map(|(src, dest, size, mtime)| {
            copy::ensure_copied(src, dest, *size, *mtime, cfg.dry_run, cfg.verbose)
        })
        .collect();
    for result in copy_results {
        match result {
            Ok(copy::CopyAction::UpToDate) => stats.files_up_to_date += 1,
            Ok(copy::CopyAction::Copied) => stats.files_copied += 1,
            Err(err) => stats.errors.push(format!("{err:#}")),
        }
    }

    // 5. Unless --no-delete, remove everything the desired state does not
    //    want, so the destination mirrors the selected view exactly.
    if !no_delete {
        let keep: HashSet<PathBuf> = pairs.iter().map(|(_, d, _, _)| d.clone()).collect();
        let (removed, dirs) = clean::reconcile_tree(target, &keep, cfg.dry_run, cfg.verbose)?;
        stats.files_removed += removed.len();
        stats.dirs_removed += dirs;
    } else {
        // --no-delete keeps files the view no longer contains, but the
        // in-progress copies of a crashed run are our litter, not user
        // data: remove them.
        stats.files_removed += copy::remove_stale_temps(target, cfg.dry_run, cfg.verbose)?;
    }

    Ok(stats)
}

/// Resolve the sync destination and refuse layouts that could write into
/// the source tree or a link output directory.
pub fn prepare_sync_target(
    source: &Path,
    formats_root: &Path,
    max_qual_root: &Path,
    target: PathBuf,
) -> Result<PathBuf> {
    let target = resolve_existing(&target)?;
    check_sync_layout(source, &[formats_root, max_qual_root], &target)?;
    Ok(target)
}

fn check_sync_layout(source: &Path, link_roots: &[&Path], target: &Path) -> Result<()> {
    if source == target {
        bail!(
            "the source directory ({}) is the same as the sync target; refusing to run",
            source.display()
        );
    }
    if is_under(target, source) {
        bail!(
            "the sync target ({}) is inside the source directory; refusing to run",
            target.display()
        );
    }
    if is_under(source, target) {
        bail!(
            "the source directory ({}) is inside the sync target; refusing to run",
            source.display()
        );
    }
    // A target that overlaps a link output directory would either copy the
    // derived tree back on top of the links, or (mirror mode) delete the
    // link layout as "stale".
    for root in link_roots {
        if target == *root {
            bail!(
                "the sync target ({}) is the same as a link output directory ({}); refusing to run",
                target.display(),
                root.display()
            );
        }
        if is_under(target, root) {
            bail!(
                "the sync target ({}) is inside a link output directory ({}); refusing to run",
                target.display(),
                root.display()
            );
        }
        if is_under(root, target) {
            bail!(
                "a link output directory ({}) is inside the sync target ({}); refusing to run",
                root.display(),
                target.display()
            );
        }
    }
    Ok(())
}
