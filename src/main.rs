use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Error, Result, bail};
use clap::{Args, Parser};

use musman::{
    Config, SavedLink, SavedSync, Stats, SyncSelection, confirm, dedup_formats, link_config_path,
    load_link, load_sync, parse_selection, relative_path, save_link, save_sync,
    selection_to_string, sync_config_path,
};

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
/// Settings can be repeated on the command line or saved in a hidden
/// `.musman.toml` file: the link config lives at `<source>/.musman.toml`,
/// the sync config at `<target>/.musman.toml`. `--no-config` ignores and
/// suppresses the relevant file; `--yes` answers configuration prompts.
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
    /// Directory containing the original music files.
    #[arg(short = 's', long = "source", default_value = ".", value_name = "DIR")]
    source: PathBuf,

    /// A managed format (e.g. flac, mp3). Repeatable; the first one given
    /// is the highest quality. May be omitted when a link config supplies it.
    #[arg(short = 'f', long = "format", value_name = "FORMAT")]
    formats: Option<Vec<String>>,

    /// Root for the per-format link trees. For `link` the default is
    /// `./formats`; for `sync` the default is `<source>/formats`.
    #[arg(short = 'i', long = "formats", value_name = "DIR")]
    formats_root: Option<PathBuf>,

    /// Root for the best-quality link tree (kept non-empty for syncthing).
    /// For `link` the default is `./max_qual`; for `sync` the default is
    /// `<source>/max_qual`.
    #[arg(short = 'm', long = "max-qual", value_name = "DIR")]
    max_qual_root: Option<PathBuf>,

    /// Print one line per file action.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Report planned actions without touching the disk.
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Ignore this subcommand's config file and do not create or update it.
    #[arg(long = "no-config")]
    no_config: bool,

    /// Answer configuration prompts with yes.
    #[arg(long = "yes")]
    yes: bool,
}

#[derive(Args)]
struct LinkArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("what").multiple(false).args(["only", "best"]))]
struct SyncArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Target directory to copy the selected view into (e.g. a removable
    /// device mount point).
    #[arg(short = 't', long = "target", required = true, value_name = "DIR")]
    target: PathBuf,

    /// Copy all source files of one managed format (e.g. `--only flac`),
    /// keeping their source-relative paths.
    #[arg(long = "only", value_name = "EXT")]
    only: Option<String>,

    /// Copy the best-quality file of each track, per the format order.
    #[arg(long = "best")]
    best: bool,

    /// Keep files the selected view no longer contains (no stale removal).
    /// Pass `--no-delete=false` to override a saved `no_delete = true`.
    #[arg(long = "no-delete", num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
    no_delete: Option<bool>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Link(link) => handle_link(&link.common),
        Command::Sync(sync) => handle_sync(
            &sync.common,
            &sync.target,
            sync.only.as_deref(),
            sync.best,
            sync.no_delete,
        ),
    }
}

fn resolve_source(source: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(source)
        .with_context(|| format!("resolving source directory {}", source.display()))?;
    if !canonical.is_dir() {
        bail!("source {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

/// Resolved link settings for one run, after merging CLI, config and
/// defaults.
struct ResolvedLink {
    formats: Vec<String>,
    formats_root: PathBuf,
    max_qual_root: PathBuf,
}

fn resolve_root(
    source: &Path,
    cli: Option<&PathBuf>,
    saved: Option<&str>,
    default: &Path,
) -> Result<PathBuf> {
    let path = if let Some(cli) = cli {
        cli.clone()
    } else if let Some(saved) = saved {
        source.join(saved)
    } else {
        default.to_path_buf()
    };
    musman::resolve_existing(&path)
}

fn resolve_link_values(
    common: &CommonArgs,
    saved: Option<&SavedLink>,
    source: &Path,
    default_formats_root: &Path,
    default_max_qual_root: &Path,
) -> Result<ResolvedLink> {
    let formats = match common
        .formats
        .clone()
        .or_else(|| saved.map(|s| s.formats.clone()))
    {
        Some(raw) => {
            let formats = dedup_formats(raw);
            if formats.is_empty() {
                bail!("at least one format is required (use -f or a link config)");
            }
            formats
        }
        None => bail!("at least one format is required (use -f or a link config)"),
    };
    let formats_root = resolve_root(
        source,
        common.formats_root.as_ref(),
        saved.map(|s| s.formats_root.as_str()),
        default_formats_root,
    )?;
    let max_qual_root = resolve_root(
        source,
        common.max_qual_root.as_ref(),
        saved.map(|s| s.max_qual_root.as_str()),
        default_max_qual_root,
    )?;
    Ok(ResolvedLink {
        formats,
        formats_root,
        max_qual_root,
    })
}

fn build_config(
    common: &CommonArgs,
    source: PathBuf,
    resolved: &ResolvedLink,
    config_path: Option<PathBuf>,
) -> Option<Config> {
    match Config::new(
        source,
        resolved.formats_root.clone(),
        resolved.max_qual_root.clone(),
        resolved.formats.clone(),
        common.verbose,
        common.dry_run,
        config_path,
    ) {
        Ok(config) => Some(config),
        Err(err) => {
            eprintln!("musman: {err:#}");
            None
        }
    }
}

fn saved_link_from_resolved(source: &Path, resolved: &ResolvedLink) -> SavedLink {
    SavedLink {
        formats: resolved.formats.clone(),
        formats_root: relative_path(source, &resolved.formats_root)
            .to_string_lossy()
            .into_owned(),
        max_qual_root: relative_path(source, &resolved.max_qual_root)
            .to_string_lossy()
            .into_owned(),
    }
}

struct LinkConfigPlan {
    write: bool,
}

fn plan_link_config(
    common: &CommonArgs,
    source: &Path,
    saved: Option<&SavedLink>,
    resolved: &ResolvedLink,
) -> Result<LinkConfigPlan> {
    let mut conflicts = Vec::new();
    if let Some(saved) = saved {
        let saved_formats = dedup_formats(saved.formats.clone());
        if let Some(cli_formats) = &common.formats {
            let cli_formats = dedup_formats(cli_formats.clone());
            if cli_formats != saved_formats {
                conflicts.push(format!(
                    "formats: saved {:?} | command line {:?}",
                    saved.formats, cli_formats
                ));
            }
        }
        if let Some(cli_formats_root) = &common.formats_root {
            let saved_root = musman::resolve_existing(&source.join(&saved.formats_root))?;
            if resolved.formats_root != saved_root {
                conflicts.push(format!(
                    "formats_root: saved {:?} | command line {:?}",
                    saved.formats_root, cli_formats_root
                ));
            }
        }
        if let Some(cli_max_qual_root) = &common.max_qual_root {
            let saved_root = musman::resolve_existing(&source.join(&saved.max_qual_root))?;
            if resolved.max_qual_root != saved_root {
                conflicts.push(format!(
                    "max_qual_root: saved {:?} | command line {:?}",
                    saved.max_qual_root, cli_max_qual_root
                ));
            }
        }
    }

    let write = if common.no_config || common.dry_run {
        false
    } else if saved.is_none() {
        true
    } else if conflicts.is_empty() {
        false
    } else {
        let path = link_config_path(source);
        let prompt = format!(
            "existing link config at {} differs from the command line:\n{}\noverwrite it? The run will use the command line values either way. [y/N]",
            path.display(),
            conflicts.join("\n")
        );
        if confirm(&prompt, common.yes) {
            true
        } else {
            eprintln!(
                "musman: keeping existing link config at {}; the run uses the command line values",
                path.display()
            );
            false
        }
    };

    if common.dry_run && !common.no_config {
        if saved.is_none() {
            println!(
                "would write link config {}",
                link_config_path(source).display()
            );
        } else if !conflicts.is_empty() {
            println!(
                "would update link config {}",
                link_config_path(source).display()
            );
        }
    }

    Ok(LinkConfigPlan { write })
}

struct SyncConfigPlan {
    write: bool,
}

fn plan_sync_config(
    common: &CommonArgs,
    target: &Path,
    saved: Option<&SavedSync>,
    cli_selection: Option<&SyncSelection>,
    cli_no_delete: Option<bool>,
) -> Result<SyncConfigPlan> {
    let mut conflicts = Vec::new();
    if let Some(saved) = saved {
        let saved_selection = parse_selection(&saved.selection)?;
        if let Some(cli_selection) = cli_selection
            && selection_to_string(cli_selection) != selection_to_string(&saved_selection)
        {
            conflicts.push(format!(
                "selection: saved {:?} | command line {:?}",
                saved.selection,
                selection_to_string(cli_selection)
            ));
        }
        if let Some(cli_no_delete) = cli_no_delete
            && cli_no_delete != saved.no_delete
        {
            conflicts.push(format!(
                "no_delete: saved {} | command line {cli_no_delete}",
                saved.no_delete
            ));
        }
    }

    let write = if common.no_config || common.dry_run {
        false
    } else if saved.is_none() {
        true
    } else if conflicts.is_empty() {
        false
    } else {
        let path = sync_config_path(target);
        let prompt = format!(
            "existing sync config at {} differs from the command line:\n{}\noverwrite it? The run will use the command line values either way. [y/N]",
            path.display(),
            conflicts.join("\n")
        );
        if confirm(&prompt, common.yes) {
            true
        } else {
            eprintln!(
                "musman: keeping existing sync config at {}; the run uses the command line values",
                path.display()
            );
            false
        }
    };

    if common.dry_run && !common.no_config {
        if saved.is_none() {
            println!(
                "would write sync config {}",
                sync_config_path(target).display()
            );
        } else if !conflicts.is_empty() {
            println!(
                "would update sync config {}",
                sync_config_path(target).display()
            );
        }
    }

    Ok(SyncConfigPlan { write })
}

fn handle_link(common: &CommonArgs) -> ExitCode {
    let source = match resolve_source(&common.source) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    let saved_link = if common.no_config {
        None
    } else {
        match load_link(&source) {
            Ok(saved) => saved,
            Err(err) => {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        }
    };

    let default_formats_root = PathBuf::from("formats");
    let default_max_qual_root = PathBuf::from("max_qual");
    let resolved = match resolve_link_values(
        common,
        saved_link.as_ref(),
        &source,
        &default_formats_root,
        &default_max_qual_root,
    ) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    let plan = match plan_link_config(common, &source, saved_link.as_ref(), &resolved) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    };

    let config = match build_config(common, source.clone(), &resolved, None) {
        Some(config) => config,
        None => return ExitCode::from(2),
    };

    let stats = match musman::run(&config) {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    };

    if stats.had_errors() || !plan.write {
        return execute(Ok(stats));
    }

    let saved = saved_link_from_resolved(&source, &resolved);
    if let Err(err) = save_link(&source, &saved) {
        eprintln!("musman: {err:#}");
        return ExitCode::from(1);
    }

    execute(Ok(stats))
}

fn handle_sync(
    common: &CommonArgs,
    target: &Path,
    only: Option<&str>,
    best: bool,
    cli_no_delete: Option<bool>,
) -> ExitCode {
    let source = match resolve_source(&common.source) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    let link_provided =
        common.formats.is_some() || common.formats_root.is_some() || common.max_qual_root.is_some();

    let saved_link = if common.no_config {
        None
    } else {
        match load_link(&source) {
            Ok(saved) => saved,
            Err(err) => {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        }
    };

    if !link_provided && saved_link.is_none() {
        eprintln!(
            "musman: no link config at {} and no link settings on the command line; run `musman link` first or pass -f/-i/-m",
            link_config_path(&source).display()
        );
        return ExitCode::from(2);
    }

    let default_formats_root = source.join("formats");
    let default_max_qual_root = source.join("max_qual");
    let resolved = match resolve_link_values(
        common,
        saved_link.as_ref(),
        &source,
        &default_formats_root,
        &default_max_qual_root,
    ) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(2);
        }
    };

    let config = match build_config(common, source.clone(), &resolved, None) {
        Some(config) => config,
        None => return ExitCode::from(2),
    };

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

    let cli_selection = if best {
        Some(SyncSelection::Best)
    } else {
        only.map(|ext| SyncSelection::Format(ext.to_string()))
    };

    let saved_sync = if common.no_config {
        None
    } else {
        match load_sync(&target) {
            Ok(saved) => saved,
            Err(err) => {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        }
    };

    let saved_selection = match saved_sync
        .as_ref()
        .map(|s| parse_selection(&s.selection))
        .transpose()
    {
        Ok(selection) => selection,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    };

    let selection = match (cli_selection.as_ref(), saved_selection) {
        (Some(cli), _) => cli.clone(),
        (None, Some(saved)) => saved,
        (None, None) => {
            eprintln!("musman: a selection is required (use --only or --best)");
            return ExitCode::from(2);
        }
    };

    if let SyncSelection::Format(ext) = &selection
        && !config.format_order.iter().any(|f| f == ext)
    {
        eprintln!(
            "musman: --only {ext} is not one of the managed formats: {}",
            config.format_order.join(", ")
        );
        return ExitCode::from(2);
    }

    let no_delete =
        cli_no_delete.unwrap_or_else(|| saved_sync.as_ref().map(|s| s.no_delete).unwrap_or(false));

    if link_provided && !common.dry_run {
        let prompt = format!(
            "link settings were provided; musman will link the library (updating derived trees and the link config at {}) and then sync to {}. Continue? [y/N]",
            source.display(),
            target.display()
        );
        if !confirm(&prompt, common.yes) {
            eprintln!("musman: aborting; no changes made.");
            return ExitCode::from(2);
        }
    }
    if link_provided && common.dry_run {
        println!("would link and sync");
    }

    let mut link_stats = None;
    if link_provided {
        let link_plan = match plan_link_config(common, &source, saved_link.as_ref(), &resolved) {
            Ok(plan) => plan,
            Err(err) => {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        };

        let link_config = match build_config(common, source.clone(), &resolved, None) {
            Some(config) => config,
            None => return ExitCode::from(2),
        };

        let stats = match musman::run(&link_config) {
            Ok(stats) => stats,
            Err(err) => {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        };

        if stats.had_errors() {
            return execute(Ok(stats));
        }

        if link_plan.write && !common.dry_run {
            let saved = saved_link_from_resolved(&source, &resolved);
            if let Err(err) = save_link(&source, &saved) {
                eprintln!("musman: {err:#}");
                return ExitCode::from(1);
            }
        }

        link_stats = Some(stats);
    }

    let sync_plan = match plan_sync_config(
        common,
        &target,
        saved_sync.as_ref(),
        cli_selection.as_ref(),
        cli_no_delete,
    ) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    };

    let sync_config_path = if common.no_config {
        None
    } else {
        Some(sync_config_path(&target))
    };
    let sync_config = match build_config(common, source.clone(), &resolved, sync_config_path) {
        Some(config) => config,
        None => return ExitCode::from(2),
    };

    let stats = match musman::run_sync(&sync_config, &target, &selection, no_delete) {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    };

    if stats.had_errors() {
        if let Some(mut total) = link_stats.take() {
            total.add(stats);
            return execute(Ok(total));
        }
        return execute(Ok(stats));
    }

    if sync_plan.write && !common.dry_run {
        let saved = SavedSync {
            selection: selection_to_string(&selection),
            no_delete,
        };
        if let Err(err) = save_sync(&target, &saved) {
            eprintln!("musman: {err:#}");
            return ExitCode::from(1);
        }
    }

    match link_stats {
        Some(mut total) => {
            total.add(stats);
            execute(Ok(total))
        }
        None => execute(Ok(stats)),
    }
}

/// Print a run's summary and map its outcome to an exit code.
fn execute(stats: Result<Stats, Error>) -> ExitCode {
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
