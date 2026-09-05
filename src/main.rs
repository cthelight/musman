use std::process::ExitCode;

use clap::Parser;

/// Maintain hard-link views of a multi-format music library.
///
/// musman keeps two derived trees in sync with a source library:
///
///   <formats>/<ext>/<relative path>   one hard link per source file
///   <max-qual>/<path>.<best ext>      one hard link per track, best format wins
///
/// Stale links (whose source file was deleted or replaced) are removed.
/// The source tree is never modified.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// A managed format (e.g. flac, mp3). Repeatable; the first one given
    /// is the highest quality.
    #[arg(short = 'f', long = "format", required = true, value_name = "FORMAT")]
    formats: Vec<String>,

    /// Directory containing the original music files.
    #[arg(short = 's', long = "source", default_value = ".", value_name = "DIR")]
    source: std::path::PathBuf,

    /// Root for the best-quality link tree (kept non-empty for syncthing).
    #[arg(
        short = 'm',
        long = "max-qual",
        default_value = "max_qual",
        value_name = "DIR"
    )]
    max_qual: std::path::PathBuf,

    /// Root for the per-format link trees.
    #[arg(
        short = 'i',
        long = "formats",
        default_value = "formats",
        value_name = "DIR"
    )]
    formats_root: std::path::PathBuf,

    /// Print one line per file action.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Report planned actions without touching the disk.
    #[arg(long = "dry-run")]
    dry_run: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let config = match musman::Config::new(
        args.source,
        args.formats_root,
        args.max_qual,
        args.formats,
        args.verbose,
        args.dry_run,
    ) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    match musman::run(&config) {
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
