# musman

Fast hard-link manager for multi-format music libraries, written in Rust.

musman keeps two *derived* trees in sync with a source music library by
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

## Building

```sh
cargo build --release
```

The binary lands in `target/release/musman`.

## Usage

```
musman -f flac -f mp3 -f ogg [options]

  -f, --format <FORMAT>   managed format; repeatable, first = highest quality (required)
  -s, --source <DIR>      directory containing the original music files   [default: .]
  -m, --max-qual <DIR>    root for the best-quality link tree              [default: max_qual]
  -i, --formats <DIR>     root for the per-format link trees               [default: formats]
  -v, --verbose           print one line per file action
      --dry-run           report planned actions without touching the disk
```

Examples:

```sh
# From inside the music directory, with the default layout:
musman -f flac -f mp3 -f ogg

# Explicit roots, e.g. for a library on NFS:
musman -s /mnt/music -i /mnt/music/formats -m /mnt/music/max_qual -f flac -f mp3

# See what would happen without changing anything:
musman -s /mnt/music -f flac -f mp3 --dry-run -v
```

Exit codes: `0` success, `1` runtime error (some files could not be
linked or removed; they are listed on stdout), `2` usage error or a
refused layout.

### Safety

- Only the source tree's regular files are read; nothing is ever
  written, removed or renamed inside it.
- Only hard links are created. Only files inside the two output trees
  are ever removed.
- Symlinks, hidden files and dotted titles are handled like `find`; a
  name without a usable extension (e.g. `README`, `.hidden`) is
  ignored, and the *last* dot wins (`1. Title.flac` is a `flac`).
- Refused layouts (exit 2): source equal to or inside an output root,
  identical or nested output roots.
- The `max_qual` tree keeps a `.stfolder` directory so syncthing does
  not drop the folder when it becomes empty. It is created once and
  never touched again (not walked, not removed, mtime preserved).

### Dry run

`--dry-run` plans the whole reconciliation and prints
`would create` / `would replace` / `would remove` / `would prune` lines
plus the final summary, without creating or deleting anything (not even
the output roots).

## Performance notes (NFS)

The design minimizes network round trips, which is what matters on a
slow mount:

- The source tree is walked once, in parallel, using the `d_type`
  field that `getdents` returns on Linux: classifying a file vs. a
  directory costs no extra syscall when the filesystem fills `d_type`
  in (most NFS servers do).
- Each source file's `(dev, ino)` identity is captured during the scan.
  The link phase never re-stats the source: a destination is only
  checked against the already-known identity.
- Output trees are reconciled in a single pass per tree: one `readdir`
  per directory discovers stale files and, bottom-up, the directories
  that became empty. Steady-state re-runs (nothing changed) cost
  roughly one `readdir`/`stat` per existing entry and create nothing.
- Link creation is parallel; missing parent directories are created
  on demand (first run only) and concurrent creation is tolerated.
- Stale-link detection compares inodes; a source file that was deleted
  and re-created under the same name (new inode) is detected and its
  links re-pointed.

## Tests

```sh
cargo test
```

- 26 unit tests for the walker, scanner, linker, name parsing and tree
  reconciliation.
- 20 end-to-end tests (`tests/integration.rs`) that run the compiled
  binary against real trees shaped like real libraries
  (`artist/album/song.ext`, albums of 1–15 songs) and assert exact file
  sets, directory structure, hard-link inode identity, exit codes and
  side effects: idempotency, mutation reconciliation, sparse/dense
  layouts, odd filenames, unsafe-layout refusals, `.stfolder`
  persistence, dry-run, a seeded randomized property test against an
  independent expected-state builder, a differential test against
  `legacy/create_music_links.sh`, and a 15,000-song performance smoke.
