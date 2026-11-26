use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Error;
use clap::{Args, Parser};

/// Manage a multi-format music library.
///
/// musman works from a read-only source library and offers two ways to
/// present a view of it:
///
///   link   maintain hard-link trees (one per format, and one best-quality
///          tree per track) on the same filesystem as the source
///   sync   incrementally copy a selected view to a target directory, such
///          as a removable device
///
/// The source tree is never modified.
#[derive(Parser)]
#[command(version, about, subcommand_required = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    /// Maintain hard-link views of the library (same-filesystem, no data copy).
    Link(LinkArgs),
    /// Incrementally copy a selected view of the library to a target directory.
    Sync(SyncArgs),
}

/// Arguments shared by the `link` and `sync` subcommands.
#[derive(Args)]
struct CommonArgs {
    /// A managed format (e.g. flac, mp3). Repeatable; the first one given
    /// is the highest quality.
    #[arg(short = 'f', long = "format", required = true, value_name = "FORMAT")]
    formats: Vec<String>,

    /// Directory containing the original music files.
    #[arg(short = 's', long = "source", default_value = ".", value_name = "DIR")]
    source: std::path::PathBuf,

    /// Root for the per-format link trees. For `link` the default is
    /// `./formats`; for `sync` the default is `<source>/formats` (the
    /// derived tree is excluded from the scan, never created).
    #[arg(short = 'i', long = "formats", value_name = "DIR")]
    formats_root: Option<std::path::PathBuf>,

    /// Root for the best-quality link tree (kept non-empty for syncthing).
    /// For `link` the default is `./max_qual`; for `sync` the default is
    /// `<source>/max_qual` (excluded from the scan, never created).
    #[arg(short = 'm', long = "max-qual", value_name = "DIR")]
    max_qual: Option<std::path::PathBuf>,

    /// Print one line per file action.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Report planned actions without touching the disk.
    #[arg(long = "dry-run")]
    dry_run: bool,
}

#[derive(Args)]
struct LinkArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("what").required(true).multiple(false).args(["only", "best"]))]
struct SyncArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Target directory to copy the selected view into (e.g. a removable
    /// device mount point).
    #[arg(short = 't', long = "target", required = true, value_name = "DIR")]
    target: std::path::PathBuf,

    /// Copy all source files of one managed format (e.g. `--only flac`),
    /// keeping their source-relative paths.
    #[arg(long = "only", value_name = "EXT")]
    only: Option<String>,

    /// Copy the best-quality file of each track, per the format order.
    #[arg(long = "best")]
    best: bool,

    /// Keep files the selected view no longer contains (no stale removal).
    #[arg(long = "no-delete")]
    no_delete: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Link(link) => run_link(&link.common),
        Command::Sync(sync) => run_sync(
            &sync.common,
            &sync.target,
            sync.only.as_deref(),
            sync.best,
            sync.no_delete,
        ),
    }
}

/// Build a validated `Config` from the shared arguments, exiting 2 on
/// a usage or layout problem.
fn make_config(
    common: &CommonArgs,
    formats_root: PathBuf,
    max_qual: PathBuf,
) -> Option<musman::Config> {
    match musman::Config::new(
        common.source.clone(),
        formats_root,
        max_qual,
        common.formats.clone(),
        common.verbose,
        common.dry_run,
    ) {
        Ok(config) => Some(config),
        Err(err) => {
            eprintln!("musman: {err:#}");
            None
        }
    }
}

/// The default for one of `sync`'s `-i`/`-m`: the derived tree at the top
/// of the source. Falls back to the bare directory name when the source
/// cannot be resolved (`Config::new` then reports the real error).
fn sync_default_root(source: &Path, name: &str) -> PathBuf {
    fs::canonicalize(source)
        .ok()
        .map(|canonical| canonical.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn run_link(common: &CommonArgs) -> ExitCode {
    // `link` creates the trees it is given, so the defaults are relative
    // to the current directory (the classic "run from inside the library").
    let formats_root = common
        .formats_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("formats"));
    let max_qual = common
        .max_qual
        .clone()
        .unwrap_or_else(|| PathBuf::from("max_qual"));
    let Some(config) = make_config(common, formats_root, max_qual) else {
        return ExitCode::from(2);
    };
    execute(musman::run(&config))
}

fn run_sync(
    common: &CommonArgs,
    target: &Path,
    only: Option<&str>,
    best: bool,
    no_delete: bool,
) -> ExitCode {
    // `sync` does not create the trees named by `-i`/`-m`; they are the
    // derived `link` trees that live inside the source and must be skipped
    // while scanning. The defaults are therefore anchored at the source,
    // not the CWD, so a library that already contains a `formats/` or
    // `max_qual/` tree is never copied back into the target.
    let formats_root = common
        .formats_root
        .clone()
        .unwrap_or_else(|| sync_default_root(&common.source, "formats"));
    let max_qual = common
        .max_qual
        .clone()
        .unwrap_or_else(|| sync_default_root(&common.source, "max_qual"));
    let Some(config) = make_config(common, formats_root, max_qual) else {
        return ExitCode::from(2);
    };

    // `--only` must name a managed format.
    if let Some(ext) = only.filter(|ext| !config.format_order.iter().any(|f| f == ext)) {
        eprintln!(
            "musman: --only {ext} is not one of the managed formats: {}",
            config.format_order.join(", ")
        );
        return ExitCode::from(2);
    }

    // Resolve the target and refuse layouts that could write into the
    // source or a link output directory.
    let target = match musman::prepare_sync_target(
        &config.source,
        &config.formats_root,
        &config.max_qual_root,
        target.to_path_buf(),
    ) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    let selection = if best {
        musman::SyncSelection::Best
    } else {
        musman::SyncSelection::Format(
            only.expect("either --only or --best is required")
                .to_string(),
        )
    };

    execute(musman::run_sync(&config, &target, &selection, no_delete))
}

/// Print a run's summary and map its outcome to an exit code.
fn execute(stats: Result<musman::Stats, Error>) -> ExitCode {
    match stats {
        Ok(stats) => {
            println!("{}", stats.summary());
            if stats.had_errors() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("musman: {err:#}");
            ExitCode::from(1)
        }
    }
}
