use std::path::Path;
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

    /// Root for the per-format link trees.
    #[arg(
        short = 'i',
        long = "formats",
        default_value = "formats",
        value_name = "DIR"
    )]
    formats_root: std::path::PathBuf,

    /// Root for the best-quality link tree (kept non-empty for syncthing).
    #[arg(
        short = 'm',
        long = "max-qual",
        default_value = "max_qual",
        value_name = "DIR"
    )]
    max_qual: std::path::PathBuf,

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
fn make_config(common: &CommonArgs) -> Option<musman::Config> {
    match musman::Config::new(
        common.source.clone(),
        common.formats_root.clone(),
        common.max_qual.clone(),
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

fn run_link(common: &CommonArgs) -> ExitCode {
    let Some(config) = make_config(common) else {
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
    let Some(config) = make_config(common) else {
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

    // Resolve the target and refuse layouts that could write into the source.
    let target = match musman::prepare_sync_target(&config.source, target.to_path_buf()) {
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
