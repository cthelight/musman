# musman

Fast manager for multi-format music libraries, written in Rust.

musman offers two operations over a source music library:

- **`link`** — keep two *derived* trees in sync with the source by
  maintaining hard links. Fast and same-filesystem; no data is copied.
- **`sync`** — incrementally *copy* a chosen view of the library to a
  target directory, touching only the files that changed. Use this to
  get a library onto a removable drive or a different filesystem where
  hard links are impossible (FAT32/exFAT USB sticks, network targets,
  a phone's storage, …).

Repeated settings can be stored in hidden `.musman.toml` files: a
successful `link` saves its link settings at `<source>/.musman.toml`,
and a successful `sync` saves its view settings at
`<target>/.musman.toml`, so later runs can often be bare reruns.

## link: derived hard-link trees

`link` keeps two *derived* trees in sync with a source music library by
maintaining hard links:

```
<formats>/<ext>/<relative path>      one hard link per source file, grouped by format
<max-qual>/<relative path>.<ext>     one hard link per track, best available format wins
```

For example, with `flac` preferred over `mp3` and `ogg`:

```
source/Artist A/Album/01 Song.flac   source/Artist A/Album/01 Song.mp3
        |                                  |
        +-- formats/flac/Artist A/Album/01 Song.flac
        +-- max_qual/Artist A/Album/01 Song.flac     (best format wins)

source/Artist B/Album/02 Song.mp3
        +-- formats/mp3/Artist B/Album/02 Song.mp3
        +-- max_qual/Artist B/Album/02 Song.mp3      (no flac available)
```

The source **music files** are never modified. The only file `link` may
create or update inside the source directory is the hidden config
`<source>/.musman.toml`, unless `--no-config` is used. The two output
trees are pure derived artifacts: on every run musman computes the
desired end state from a single pass over the source, creates missing
links, re-points stale ones, and removes everything else (including
subdirectories of formats that are no longer managed), pruning the
directories left empty. After a run the output trees match the source
tree exactly.

musman replaces `legacy/create_music_links.sh`, which had the same
semantics but ran one `find` plus one `bash`/`ln -f` process per format
and per file.

## sync: incremental copy to a target

`sync` copies the selected files to a target directory and is the tool
to use when the destination is a different device or filesystem. Unlike
`link`, it **copies bytes** (so it works across filesystems and on
FAT32/exFAT) and it **re-copies only what changed**, so re-running
against an already-synced drive is cheap.

Selection is *exactly one* of:

- `--only EXT` — copy every source file of one managed format, keeping
  the source-relative path (e.g. `--only flac` →
  `target/Artist/Album/01 Song.flac`).
- `--best` — copy the best-quality file of each track (the highest
  `--format` that exists for that track), named `<track>.<ext>`.

The selection may also come from the target's sync config
(`selection = "best"` or `selection = "only:<ext>"`); a CLI selection
always wins for the current run and may update the saved config.

### Change detection

`sync` decides whether a destination file is up to date from **size and
mtime only** — it never re-reads file contents and never re-stats the
source (both are captured during the single scan). This keeps steady
re-runs cheap on slow mounts:

- A destination is re-copied only if its size or mtime differs from the
  source. mtime is compared with a 2 second tolerance to absorb FAT32's
  coarse timestamp granularity.
- The source's mtime is preserved on the destination: each file is
  written to a temporary file, its mtime set, then atomically renamed
  into place, so a crash mid-copy never leaves a truncated destination.
- By default `sync` **mirrors** the target: it adds missing files,
  updates changed ones, and removes files that are no longer selected,
  pruning the directories that become empty. `--no-delete` keeps files
  that are no longer selected (add/update only). Bare `--no-delete`
  means `--no-delete=true`; pass `--no-delete=false` to override a saved
  `no_delete = true`. The saved `no_delete` value is used when the flag
  is absent.

When `sync` is given any link setting (`-f`, `-i` or `-m`), it first
runs the equivalent `link` operation and then syncs the result. This is
announced with a `Continue? [y/N]` prompt before anything changes;
`--yes` accepts it, and `n` aborts with exit code `2` and no changes.
`--dry-run` reports `would link and sync` without prompting.

## Configuration

Two hidden, human-editable TOML files are used:

- `<source>/.musman.toml` — the link config, describing the library:

  ```toml
  formats = ["flac", "mp3", "ogg"] # first = highest quality
  formats_root = "formats"         # relative to <source>
  max_qual_root = "max_qual"       # relative to <source>
  ```

- `<target>/.musman.toml` — the sync config, describing one target view:

  ```toml
  selection = "best" # or "only:flac"
  no_delete = false
  ```

Link config roots are stored relative to the source, using `..` when
necessary, so the same source config keeps working if the library is
mounted at another path. The sync config is kept in the target keep-set
during mirroring and is not removed by `sync`.

Resolution order:

- link settings: CLI → `<source>/.musman.toml` → subcommand defaults
  (`link` defaults to CWD-relative `formats`/`max_qual`; `sync` defaults
  to `<source>/formats` and `<source>/max_qual`);
- sync selection: CLI `--only`/`--best` → `<target>/.musman.toml` →
  error if absent;
- `no_delete`: CLI `--no-delete[=BOOL]` → `<target>/.musman.toml` →
  `false`.

CLI values always drive the current run. A missing config is written
automatically after a successful run. If an existing config conflicts
with the CLI, musman prompts before overwriting it; the run uses the CLI
values even if the answer is `n`. `--yes` answers configuration prompts
yes; on a non-terminal stdin without `--yes`, prompts assume `n` so
unattended runs never block or silently overwrite saved settings.

`--no-config` suppresses config handling for the current subcommand:
`link` neither reads nor writes `<source>/.musman.toml`, and `sync`
neither reads nor writes the source link config or the target sync
config. `sync --no-config` therefore requires link settings on the
command line. `--dry-run` never writes or updates config files; it
prints `would write ...` or `would update ...` when a config would
change.

## Building

```sh
cargo build --release
```

The binary lands in `target/release/musman`.

## Usage

```
musman link [-f FORMAT]... [options]
musman sync -t TARGET [--only EXT | --best] [-f FORMAT]... [options]

Common options:
  -s, --source <DIR>      directory containing the original music files   [default: .]
  -f, --format <FORMAT>   managed format; repeatable, first = highest quality
                          (may be omitted when a link config supplies it)
  -i, --formats <DIR>     per-format link-tree root
                          [link default: formats, sync default: <source>/formats]
  -m, --max-qual <DIR>    best-quality link-tree root
                          [link default: max_qual, sync default: <source>/max_qual]
      --dry-run           report planned actions without touching the disk
  -v, --verbose           print one line per file action
      --no-config         ignore and suppress the relevant .musman.toml file(s)
      --yes               answer configuration prompts with yes

sync options:
  -t, --target <DIR>      destination root for the copied tree   (required)
      --only <EXT>        sync exactly one managed format
      --best              sync the best-quality file per track
      --no-delete[=BOOL]  keep files that are no longer selected (no mirroring)
                          [bare flag = true; use --no-delete=false to force mirroring]
```

On `sync`, `-i`/`-m` do not merely exclude trees from the source scan;
together with `-f`, or on their own, they make `sync` perform the
equivalent `link` operation first and then sync. If only `-i`/`-m` are
given, the managed formats still come from the CLI or the link config.
By default the sync exclusion roots point at `<source>/formats` and
`<source>/max_qual`; if your exclusion roots point somewhere else but the
source still contains such a tree, musman warns that its files will be
treated as source content.

Examples:

```sh
# First link run: saves /mnt/music/.musman.toml.
musman link -s /mnt/music -f flac -f mp3

# Later link run: reads formats and roots from the saved link config.
musman link -s /mnt/music

# First sync run with link settings: links, then syncs; saves both configs.
musman sync -s /mnt/music -t /run/media/you/MUSIC --best -f flac -f mp3 --yes

# Later pure sync run: reads the link config and the target sync config.
musman sync -s /mnt/music -t /run/media/you/MUSIC

# Override the saved selection and no_delete for this run, and update
# the saved sync config to match.
musman sync -s /mnt/music -t /run/media/you/MUSIC --only flac --no-delete=false --yes

# One-off sync without reading or writing any config files.
musman sync -s /mnt/music -t /run/media/you/MUSIC --only flac -f flac -f mp3 --no-config

# See what a sync would do without changing anything:
musman sync -s /mnt/music -t /run/media/you/MUSIC --best -f flac -f mp3 --dry-run -v
```

Exit codes: `0` success, `1` runtime or config read/write error (failed
file actions are listed on stdout), `2` usage error, a refused layout,
or a declined link-then-sync prompt.

### Safety

- Only the source tree's regular music files are read; the only source
  file `musman` creates or updates is `<source>/.musman.toml`, unless
  `--no-config` is used.
- `link` creates only hard links and removes only files inside the two
  output trees. `sync` writes only inside the target and (when
  mirroring) removes only files inside the target, never deleting the
  target's own `.musman.toml`.
- Symlinks, hidden files and dotted titles are handled like `find`; a
  name without a usable extension (e.g. `README`, `.hidden`) is
  ignored, and the *last* dot wins (`1. Title.flac` is a `flac`).
- Refused layouts (exit 2): `link` refuses the source equal to or
  inside an output root, or identical/nested output roots; `sync`
  refuses a target that equals, contains, or is contained by the
  source or by either link output directory (`-i`/`-m`).
- Directories named `.stfolder` (syncthing markers) are ignored
  entirely, whether or not they exist: never created, never removed,
  never touched (not walked, mtime preserved).

### Dry run

`--dry-run` plans the whole operation and prints `would create` /
`would replace` / `would remove` / `would prune` (for `link`) or
`would copy` / `ok` (up-to-date files) / `would remove` / `would prune`
(for `sync`) lines plus the final summary, without creating or
deleting anything (not even the output/target roots). It also prints
`would write ...` / `would update ...` when a config file would be
written or changed, and `would link and sync` for a sync command that
includes link settings.

## Performance notes (NFS)

The design minimizes network round trips, which is what matters on a
slow mount:

- The source tree is walked once, in parallel, using the `d_type`
  field that `getdents` returns on Linux: classifying a file vs. a
  directory costs no extra syscall when the filesystem fills `d_type`
  in (most NFS servers do).
- Each source file's `(dev, ino, size, mtime)` identity is captured
  during the scan. The link phase never re-stats the source: a
  destination is only checked against the already-known identity.
- Output trees are reconciled in a single pass per tree: one `readdir`
  per directory discovers stale files and, bottom-up, the directories
  that became empty. Steady-state re-runs (nothing changed) cost
  roughly one `readdir`/`stat` per existing entry and create nothing.
- `sync` makes one `stat` of each existing destination (a single syscall
  on the target device) and only reads a source file when the copy is
  actually needed, so re-syncing an unchanged drive costs no source
  reads.
- Link creation is parallel; missing parent directories are created
  on demand (first run only) and concurrent creation is tolerated.
- Stale-link detection compares inodes; a source file that was deleted
  and re-created under the same name (new inode) is detected and its
  links re-pointed.

## Tests

```sh
cargo test
```

- 42 unit tests for the walker, scanner, linker, copier, name parsing,
  tree reconciliation, and the `.musman.toml` config store.
- 52 end-to-end tests (`tests/integration.rs`) that run the compiled
  binary against real trees shaped like real libraries
  (`artist/album/song.ext`, albums of 1–15 songs) and assert exact file
  sets, directory structure, hard-link inode identity, exit codes and
  side effects: idempotency, mutation reconciliation, sparse/dense
  layouts, odd filenames, unsafe-layout refusals, `.stfolder` being
  left alone, dry-run, config write/conflict/prompt behavior,
  `sync` add/update/remove and mtime preservation, and a seeded
  randomized property test against an independent expected-state
  builder, a differential test against `legacy/create_music_links.sh`,
  and a 15,000-song performance smoke.
