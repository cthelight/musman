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

The source tree is **never modified**. The two output trees are pure
derived artifacts: on every run musman computes the desired end state
from a single pass over the source, creates missing links, re-points
stale ones, and removes everything else (including subdirectories of
formats that are no longer managed), pruning the directories left empty.
After a run the output trees match the source tree exactly.

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
  that are no longer selected (add/update only).

## Building

```sh
cargo build --release
```

The binary lands in `target/release/musman`.

## Usage

```
musman link -f flac -f mp3 [options]
musman sync -f flac -f mp3 -t TARGET (--only EXT | --best) [options]

Common options:
  -f, --format <FORMAT>   managed format; repeatable, first = highest quality (required)
  -s, --source <DIR>      directory containing the original music files   [default: .]
      --dry-run           report planned actions without touching the disk
  -v, --verbose           print one line per file action

link options:
  -m, --max-qual <DIR>    root for the best-quality link tree              [default: max_qual]
  -i, --formats <DIR>     root for the per-format link trees               [default: formats]

sync options:
  -t, --target <DIR>      destination root for the copied tree   (required)
      --only <EXT>        sync exactly one managed format
      --best              sync the best-quality file per track
      --no-delete         keep files that are no longer selected (no mirroring)
  -i, --formats <DIR>     derived per-format tree to exclude from the source scan [default: <source>/formats]
  -m, --max-qual <DIR>    derived best-quality tree to exclude from the source scan [default: <source>/max_qual]
```

On `sync`, `-i`/`-m` do not create trees; they name the derived `link`
trees that live inside the source directory and are ignored while
scanning, so a library that already contains a `formats/`/`max_qual/`
tree is never copied back into itself. By default they point at
`<source>/formats` and `<source>/max_qual`; if your exclusion roots point
somewhere else but the source still contains such a tree, musman warns
that its files will be treated as source content.

Examples:

```sh
# Maintain the derived hard-link trees in a library on NFS:
musman link -s /mnt/music -i /mnt/music/formats -m /mnt/music/max_qual -f flac -f mp3

# Copy the best quality of a whole library onto a USB stick:
musman sync -s /mnt/music -t /run/media/you/MUSIC --best -f flac -f mp3 -f ogg

# Only the lossless files, and keep anything already on the stick:
musman sync -s /mnt/music -t /run/media/you/MUSIC --only flac --no-delete -f flac -f mp3

# See what a sync would do without changing anything:
musman sync -s /mnt/music -t /run/media/you/MUSIC --best -f flac -f mp3 --dry-run -v
```

Exit codes: `0` success, `1` runtime error (some files could not be
linked, copied or removed; they are listed on stdout), `2` usage error
or a refused layout.

### Safety

- Only the source tree's regular files are read; nothing is ever
  written, removed or renamed inside it.
- `link` creates only hard links and removes only files inside the two
  output trees. `sync` writes only inside the target and (when
  mirroring) removes only files inside the target.
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
deleting anything (not even the output/target roots).

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

- 34 unit tests for the walker, scanner, linker, copier, name parsing
  and tree reconciliation.
- 32 end-to-end tests (`tests/integration.rs`) that run the compiled
  binary against real trees shaped like real libraries
  (`artist/album/song.ext`, albums of 1–15 songs) and assert exact file
  sets, directory structure, hard-link inode identity, exit codes and
  side effects: idempotency, mutation reconciliation, sparse/dense
   layouts, odd filenames, unsafe-layout refusals, `.stfolder` being
   left alone, dry-run, `sync` add/update/remove and mtime preservation,
   and a seeded randomized property test against an independent
   expected-state builder, a differential test against
   `legacy/create_music_links.sh`, and a 15,000-song performance smoke.
