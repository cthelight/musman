//! End-to-end integration tests for the musman binary.
//!
//! These run the compiled binary against real trees on disk and check the
//! observable results: the exact set of files and directories, their
//! inodes (hard-link identity), exit codes and side effects. The source
//! trees follow the real-world shape `artist/album/song.ext` with albums
//! of one to a dozen or so songs, so the output trees have a high
//! directory-to-file ratio. A seeded randomized property test compares
//! musman against an independent expected-state builder, and a
//! differential test compares it against the legacy shell script.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rand::SeedableRng;
use rand::prelude::*;
use rand::rngs::SmallRng;

/// Managed format order used across the tests (first = best).
const FORMATS: &[&str] = &["flac", "mp3", "ogg"];

fn musman_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_musman"))
}

/// Run the binary, returning (exit code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(musman_bin())
        .args(args)
        .output()
        .expect("spawn musman");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run the binary and require success.
fn run_ok(args: &[&str]) -> (String, String) {
    let (code, out, err) = run(args);
    assert_eq!(code, 0, "musman failed: stdout={out} stderr={err}");
    (out, err)
}

/// Create parents as needed and write a file.
fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// All regular files under `root`, as relative path -> absolute path.
fn list_files(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, path);
            }
        }
    }
    out
}

/// All directories under `root`, as sorted relative path strings
/// (the root itself is not included).
fn list_dirs(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel);
                stack.push(path);
            }
        }
    }
    out
}

/// Every proper directory prefix of the given relative file paths.
fn expected_dirs(files: &BTreeMap<String, PathBuf>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for rel in files.keys() {
        let parts: Vec<&str> = rel.split('/').collect();
        let mut prefix = String::new();
        for part in parts.iter().take(parts.len() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            out.insert(prefix.clone());
        }
    }
    out
}

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
}

/// Snapshot every file under `root`: relative path -> (inode, contents).
fn snapshot(root: &Path) -> BTreeMap<String, (u64, Vec<u8>)> {
    list_files(root)
        .into_iter()
        .map(|(rel, path)| (rel, (inode(&path), fs::read(&path).unwrap())))
        .collect()
}

/// The deterministic source tree used by most tests
/// (`artist/album/song.ext` shape, one dotted title, a hidden directory
/// and some unmanaged files):
///
/// ```text
/// Artist/Album A/01 Intro.{flac,mp3,ogg}
/// Artist/Album A/02 Outro.{mp3,ogg}
/// Artist/Album B/1. Title.{flac,mp3}
/// .hidden/secret.flac
/// Artist/notes.txt
/// README
/// ```
fn make_source_tree(src: &Path) {
    write_file(&src.join("Artist/Album A/01 Intro.flac"), "flac-a");
    write_file(&src.join("Artist/Album A/01 Intro.mp3"), "mp3-a");
    write_file(&src.join("Artist/Album A/01 Intro.ogg"), "ogg-a");
    write_file(&src.join("Artist/Album A/02 Outro.mp3"), "mp3-b");
    write_file(&src.join("Artist/Album A/02 Outro.ogg"), "ogg-b");
    write_file(&src.join("Artist/Album B/1. Title.flac"), "flac-c");
    write_file(&src.join("Artist/Album B/1. Title.mp3"), "mp3-c");
    write_file(&src.join(".hidden/secret.flac"), "flac-h");
    write_file(&src.join("Artist/notes.txt"), "txt");
    write_file(&src.join("README"), "readme");
}

/// Independent expected-state builder (deliberately reimplemented with
/// plain string handling, not the musman library).
///
/// Returns the desired output files as relative path -> source path, for
/// the formats tree and the max-qual tree. `exclude` are the output
/// roots: when they are nested inside the source tree they exist (and
/// are full of links) by the time this runs, so they must be pruned from
/// the walk just like the binary does.
fn expected_state(
    src: &Path,
    order: &[&str],
    exclude: &[PathBuf],
) -> (BTreeMap<String, PathBuf>, BTreeMap<String, PathBuf>) {
    let mut format_links: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut best: BTreeMap<String, (usize, String, PathBuf)> = BTreeMap::new();

    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if exclude
            .iter()
            .any(|ex| dir == *ex || dir.starts_with(ex.as_path()))
        {
            continue;
        }
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(dot) = name.rfind('.') else {
                continue;
            };
            if dot == 0 || dot + 1 == name.len() {
                continue;
            }
            let ext = &name[dot + 1..];
            let Some(index) = order.iter().position(|f| *f == ext) else {
                continue;
            };
            let rel = path.strip_prefix(src).unwrap();
            let rel_str = rel.to_string_lossy().into_owned();
            format_links.insert(format!("{ext}/{rel_str}"), path.clone());

            let stem = &name[..dot];
            let dir_str = rel
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let key = if dir_str.is_empty() {
                stem.to_string()
            } else {
                format!("{dir_str}/{stem}")
            };
            match best.get_mut(&key) {
                Some(current) if current.0 <= index => {}
                Some(current) => *current = (index, ext.to_string(), path.clone()),
                None => {
                    best.insert(key, (index, ext.to_string(), path.clone()));
                }
            }
        }
    }

    let mut track_links = BTreeMap::new();
    for (key, (_, ext, path)) in best {
        track_links.insert(format!("{key}.{ext}"), path);
    }
    (format_links, track_links)
}

/// Assert that `out_root` contains exactly the expected files and that
/// each one is a hard link of the recorded source file.
fn assert_tree_matches(out_root: &Path, expected: &BTreeMap<String, PathBuf>) {
    let actual = list_files(out_root);
    let actual_rels: BTreeSet<String> = actual.keys().cloned().collect();
    let expected_rels: BTreeSet<String> = expected.keys().cloned().collect();
    assert_eq!(
        actual_rels,
        expected_rels,
        "file set mismatch in {}",
        out_root.display()
    );
    for (rel, src_path) in expected {
        let actual_path = actual.get(rel).unwrap();
        assert_eq!(
            inode(actual_path),
            inode(src_path),
            "{rel} is not a hard link of its source"
        );
    }
}

/// Assert the directory structure matches the files exactly: no missing
/// and no superfluous (e.g. stale empty) directories.
fn assert_dirs_match(out_root: &Path, expected_files: &BTreeMap<String, PathBuf>) {
    let expected = expected_dirs(expected_files);
    let actual = list_dirs(out_root);
    assert_eq!(
        actual,
        expected,
        "directory structure mismatch in {}",
        out_root.display()
    );
}

/// Run musman on the given roots with all managed formats.
fn run_musman(src: &Path, formats: &Path, max_qual: &Path, extra: &[&str]) -> (String, String) {
    let mut args: Vec<String> = vec![
        "-s".into(),
        src.display().to_string(),
        "-i".into(),
        formats.display().to_string(),
        "-m".into(),
        max_qual.display().to_string(),
    ];
    for format in FORMATS {
        args.push("-f".into());
        args.push((*format).into());
    }
    args.extend(extra.iter().map(|s| s.to_string()));
    let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_ok(&args)
}

// ---------------------------------------------------------------------
// Layout and reconciliation
// ---------------------------------------------------------------------

#[test]
fn fresh_run_creates_full_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_source_tree(&src);
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert_dirs_match(&formats, &expected_formats);
    assert_eq!(
        list_dirs(&max_qual),
        expected_dirs(&expected_tracks)
            .into_iter()
            .chain(std::iter::once(".stfolder".to_string()))
            .collect()
    );

    assert!(max_qual.join(".stfolder").is_dir());
    let expected_rels: Vec<&str> = expected_formats.keys().map(|s| s.as_str()).collect();
    assert!(expected_rels.contains(&"flac/Artist/Album A/01 Intro.flac"));
    assert!(expected_rels.contains(&"flac/.hidden/secret.flac"));
    assert!(expected_rels.contains(&"mp3/Artist/Album A/02 Outro.mp3"));
    assert!(
        !expected_rels
            .iter()
            .any(|k| k.starts_with("ogg/Artist/Album B"))
    );
    assert!(!expected_rels.iter().any(|k| k.contains("notes")));
}

#[test]
fn second_run_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_source_tree(&src);
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    let before = snapshot(&formats);
    let stfolder_before = fs::metadata(max_qual.join(".stfolder")).unwrap();

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("0 created"), "expected no creations: {out}");
    assert!(
        out.contains("0 replaced"),
        "expected no replacements: {out}"
    );
    assert!(!out.contains("removed"), "expected no removals: {out}");

    assert_eq!(before, snapshot(&formats), "formats tree changed on re-run");
    let stfolder_after = fs::metadata(max_qual.join(".stfolder")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        stfolder_before.ino(),
        stfolder_after.ino(),
        ".stfolder was recreated on re-run"
    );
}

#[test]
fn removing_best_format_falls_back_in_one_run() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    write_file(&src.join("Artist/Album/01 Song.mp3"), "mp3");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    assert_eq!(
        inode(&max_qual.join("Artist/Album/01 Song.flac")),
        inode(&src.join("Artist/Album/01 Song.flac"))
    );

    fs::remove_file(src.join("Artist/Album/01 Song.flac")).unwrap();
    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("files removed"), "expected a removal: {out}");

    assert!(!max_qual.join("Artist/Album/01 Song.flac").exists());
    assert_eq!(
        inode(&max_qual.join("Artist/Album/01 Song.mp3")),
        inode(&src.join("Artist/Album/01 Song.mp3")),
        "max_qual must fall back to the next best format"
    );
    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
}

#[test]
fn adding_a_better_format_repoints_max_qual() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.mp3"), "mp3");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    assert_eq!(
        inode(&max_qual.join("Artist/Album/01 Song.mp3")),
        inode(&src.join("Artist/Album/01 Song.mp3"))
    );

    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    run_musman(&src, &formats, &max_qual, &[]);

    assert_eq!(
        inode(&max_qual.join("Artist/Album/01 Song.flac")),
        inode(&src.join("Artist/Album/01 Song.flac"))
    );
    assert!(
        !max_qual.join("Artist/Album/01 Song.mp3").exists(),
        "the worse max-qual link must be removed"
    );
}

#[test]
fn replacing_a_source_file_repoints_its_links() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let target = src.join("Artist/Album/01 Song.flac");
    write_file(&target, "old");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    let old_ino = inode(&target);
    assert_eq!(
        inode(&formats.join("flac/Artist/Album/01 Song.flac")),
        old_ino
    );
    assert_eq!(inode(&max_qual.join("Artist/Album/01 Song.flac")), old_ino);

    let new_file = src.join("Artist/Album/new.tmp");
    write_file(&new_file, "new contents");
    fs::rename(&new_file, &target).unwrap();
    let new_ino = inode(&target);
    assert_ne!(old_ino, new_ino);

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(
        out.contains("2 replaced") || out.contains("1 replaced"),
        "expected replacements: {out}"
    );
    assert_eq!(
        inode(&formats.join("flac/Artist/Album/01 Song.flac")),
        new_ino,
        "formats link must follow the new source file"
    );
    assert_eq!(
        inode(&max_qual.join("Artist/Album/01 Song.flac")),
        new_ino,
        "max-qual link must follow the new source file"
    );
}

#[test]
fn unmanaged_format_subtrees_are_cleaned() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_source_tree(&src);
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    write_file(&formats.join("wav/Artist/old.wav"), "wav");
    write_file(&formats.join("mp3/Artist/stray.txt"), "txt");
    fs::create_dir_all(formats.join("mp3/empty/")).unwrap();

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(
        out.contains("files removed"),
        "junk files must be removed: {out}"
    );

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert_dirs_match(&formats, &expected_formats);
    assert!(!formats.join("wav").exists());
    assert!(!formats.join("mp3/empty").exists());
}

#[test]
fn deleting_source_files_removes_their_links_and_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.ogg"), "ogg");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    assert!(formats.join("ogg/Artist/Album/01 Song.ogg").is_file());
    assert!(max_qual.join("Artist/Album/01 Song.ogg").is_file());

    fs::remove_file(src.join("Artist/Album/01 Song.ogg")).unwrap();
    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(
        out.contains("files removed: 2"),
        "both links must go: {out}"
    );

    assert!(
        !formats.join("ogg").exists(),
        "the whole empty format subtree must be pruned"
    );
    assert!(
        !max_qual.join("Artist/Album").exists(),
        "the empty album directory must be pruned"
    );
    assert!(max_qual.join(".stfolder").is_dir());
}

// ---------------------------------------------------------------------
// Real-world shape: sparse, deep trees
// ---------------------------------------------------------------------

#[test]
fn sparse_tree_with_single_song_albums() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");

    let mut song_count = 0usize;
    for artist in 0..30 {
        for album in 0..4 {
            let base = format!("Artist {artist:02}/Album {album:02}/01 Song");
            write_file(&src.join(format!("{base}.flac")), "flac");
            if (artist + album) % 3 == 0 {
                write_file(&src.join(format!("{base}.mp3")), "mp3");
            }
            song_count += 1;
        }
    }
    assert_eq!(song_count, 120);

    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");
    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("120 tracks"), "expected 120 tracks: {out}");

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert_dirs_match(&formats, &expected_formats);
    assert_eq!(
        list_dirs(&max_qual),
        expected_dirs(&expected_tracks)
            .into_iter()
            .chain(std::iter::once(".stfolder".to_string()))
            .collect()
    );

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("0 created"), "re-run must be a no-op: {out}");
    assert_dirs_match(&formats, &expected_formats);
}

#[test]
fn dense_and_sparse_albums_coexist() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");

    write_file(&src.join("Soloist/Only/01 Song.flac"), "flac");
    let mut n = 0;
    for song in 0..15 {
        let base = format!("Band/Greatest Hits/{song:02} Song");
        write_file(&src.join(format!("{base}.flac")), "flac");
        write_file(&src.join(format!("{base}.mp3")), "mp3");
        n += 1;
    }
    assert_eq!(n, 15);

    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");
    run_musman(&src, &formats, &max_qual, &[]);

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert_dirs_match(&formats, &expected_formats);
    assert_eq!(expected_formats.len(), 1 + 30);
    assert_eq!(expected_tracks.len(), 16);
}

#[test]
fn filenames_with_spaces_quotes_and_dots() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let base = "My Artist/Best \"Album\" 2024/01 Greatest Hit (Remastered) 9. Edit";
    write_file(&src.join(format!("{base}.flac")), "flac");
    write_file(&src.join(format!("{base}.mp3")), "mp3");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert!(formats.join(format!("flac/{base}.flac")).is_file());
    assert!(max_qual.join(format!("{base}.flac")).is_file());
}

// ---------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------

#[test]
fn source_tree_is_never_modified() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_source_tree(&src);
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");
    write_file(&formats.join("wav/junk.wav"), "junk");
    write_file(&max_qual.join("stale.flac"), "stale");

    let before = snapshot(&src);
    run_musman(&src, &formats, &max_qual, &[]);
    let after = snapshot(&src);

    assert_eq!(before, after, "the source tree was modified");
}

#[test]
fn nested_output_inside_source_is_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    write_file(&src.join("Artist/Album/01 Song.mp3"), "mp3");

    let formats = src.join("formats");
    let max_qual = src.join("max_qual");
    run_musman(&src, &formats, &max_qual, &[]);

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
    assert!(
        !formats.join("flac/formats").exists() && !max_qual.join("formats").exists(),
        "the output trees must not link themselves"
    );

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("0 created"), "re-run must be a no-op: {out}");
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
}

#[test]
fn layout_refusals_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let source = root.join("src");
    fs::create_dir_all(source.join("sub")).unwrap();

    let (code, _, err) = run(&[
        "-s",
        &source.display().to_string(),
        "-i",
        &source.display().to_string(),
        "-f",
        "flac",
    ]);
    assert_eq!(code, 2, "source == formats root must be refused");
    assert!(err.contains("same as an output directory"), "{err}");

    let (code, _, _err) = run(&[
        "-s",
        &source.display().to_string(),
        "-m",
        &source.display().to_string(),
        "-f",
        "flac",
    ]);
    assert_eq!(code, 2, "source == max-qual root must be refused");

    let sub = source.join("sub");
    let (code, _, err) = run(&[
        "-s",
        &sub.display().to_string(),
        "-i",
        &source.display().to_string(),
        "-f",
        "flac",
    ]);
    assert_eq!(code, 2, "source inside an output root must be refused");
    assert!(err.contains("inside an output directory"), "{err}");

    let (code, _, err) = run(&[
        "-s",
        &source.display().to_string(),
        "-i",
        &source.join("out").display().to_string(),
        "-m",
        &source.join("out/max_qual").display().to_string(),
        "-f",
        "flac",
    ]);
    assert_eq!(code, 2, "nested output roots must be refused");
    assert!(err.contains("nested"), "{err}");

    let (code, _, err) = run(&[
        "-s",
        &source.display().to_string(),
        "-i",
        &source.join("out").display().to_string(),
        "-m",
        &source.join("out").display().to_string(),
        "-f",
        "flac",
    ]);
    assert_eq!(code, 2, "identical output roots must be refused");
    assert!(err.contains("must differ"), "{err}");
}

#[test]
fn duplicate_format_flags_are_deduped() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    write_file(&src.join("Artist/Album/01 Song.mp3"), "mp3");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    let args: Vec<String> = vec![
        "-s".into(),
        src.display().to_string(),
        "-i".into(),
        formats.display().to_string(),
        "-m".into(),
        max_qual.display().to_string(),
        "-f".into(),
        "flac".into(),
        "-f".into(),
        "mp3".into(),
        "-f".into(),
        "flac".into(),
    ];
    let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (out, _) = run_ok(&args);
    assert!(out.contains("2 managed"), "{out}");
    let (expected_formats, expected_tracks) =
        expected_state(&src, &["flac", "mp3"], &[formats.clone(), max_qual.clone()]);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
}

// ---------------------------------------------------------------------
// syncthing marker and dry run
// ---------------------------------------------------------------------

#[test]
fn stfolder_created_once_and_never_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    write_file(&src.join("Artist/Album/01 Song.mp3"), "mp3");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    run_musman(&src, &formats, &max_qual, &[]);
    let marker = max_qual.join(".stfolder");
    assert!(marker.is_dir());
    fs::write(marker.join("conflict.txt"), "syncthing data").unwrap();
    let marker_meta_before = fs::metadata(&marker).unwrap();

    fs::remove_file(src.join("Artist/Album/01 Song.flac")).unwrap();
    run_musman(&src, &formats, &max_qual, &[]);

    let marker_meta_after = fs::metadata(&marker).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        marker_meta_before.ino(),
        marker_meta_after.ino(),
        ".stfolder must not be recreated"
    );
    assert_eq!(
        marker_meta_before.modified().unwrap(),
        marker_meta_after.modified().unwrap(),
        ".stfolder mtime must not churn"
    );
    assert_eq!(
        fs::read_to_string(marker.join("conflict.txt")).unwrap(),
        "syncthing data",
        ".stfolder contents must be untouched"
    );
}

#[test]
fn dry_run_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("Artist/Album/01 Song.flac"), "flac");
    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");

    let (out, _) = run_musman(&src, &formats, &max_qual, &["--dry-run", "-v"]);
    assert!(
        out.lines().any(|l| l.starts_with("would create")),
        "dry run must plan creations: {out}"
    );
    assert!(
        !formats.exists(),
        "dry run must not create the formats root"
    );
    assert!(
        !max_qual.exists(),
        "dry run must not create the max-qual root"
    );

    run_musman(&src, &formats, &max_qual, &[]);
    write_file(&src.join("Artist/Album/02 Song.mp3"), "mp3");
    fs::remove_file(src.join("Artist/Album/01 Song.flac")).unwrap();
    write_file(&max_qual.join("Artist/stale.flac"), "stale");

    let before = (snapshot(&formats), snapshot(&max_qual));
    let (out, _) = run_musman(&src, &formats, &max_qual, &["--dry-run", "-v"]);
    assert!(
        out.lines().any(|l| l.starts_with("would create")),
        "dry run must plan the new link: {out}"
    );
    assert!(
        out.lines().any(|l| l.starts_with("would remove")),
        "dry run must plan the stale removal: {out}"
    );
    assert_eq!(
        before,
        (snapshot(&formats), snapshot(&max_qual)),
        "dry run must not touch the disk"
    );
}

// ---------------------------------------------------------------------
// CLI behavior
// ---------------------------------------------------------------------

#[test]
fn usage_errors_exit_with_code_2() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    let (code, _, _) = run(&["-s", &src.display().to_string()]);
    assert_eq!(code, 2, "missing -f must be a usage error");

    let (code, _, _) = run(&["--definitely-not-a-flag"]);
    assert_eq!(code, 2, "unknown flags must be a usage error");
}

// ---------------------------------------------------------------------
// Randomized property test
// ---------------------------------------------------------------------

/// Generate a realistic library: `artist/album/song.ext` with albums of
/// one to fifteen songs (usually fewer) and one to three formats per
/// song, plus occasional unmanaged files.
fn generate_library(rng: &mut SmallRng, root: &Path) {
    let artist_pool = ["Artist A", "B C", "X Y Z", "Quoted \"Band\""];
    for i in 0..rng.random_range(2..=6) {
        let artist = &artist_pool[rng.random_range(0..artist_pool.len())];
        let artist_dir = root.join(format!("{artist} {i}"));
        for j in 0..rng.random_range(1..=5) {
            let album_dir = artist_dir.join(format!("Album {j}"));
            let songs = if rng.random_bool(0.4) {
                1
            } else {
                rng.random_range(1..=15)
            };
            for s in 0..songs {
                let stem = format!("{}/{s:02} Song", album_dir.display());
                let mut exts: Vec<&str> = Vec::new();
                if rng.random_bool(0.7) {
                    exts.push("flac");
                }
                if rng.random_bool(0.8) {
                    exts.push("mp3");
                }
                if rng.random_bool(0.4) {
                    exts.push("ogg");
                }
                if exts.is_empty() {
                    exts.push("mp3");
                }
                for ext in exts {
                    write_file(
                        &Path::new(&stem).with_extension(ext),
                        &format!("data-{ext}"),
                    );
                }
                if rng.random_bool(0.1) {
                    write_file(&Path::new(&stem).with_extension("txt"), "notes");
                }
            }
        }
    }
}

#[test]
fn randomized_property_test() {
    let mut rng = SmallRng::seed_from_u64(0x5EED_2026);
    for generation in 0..6 {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let formats = tmp.path().join("formats");
        let max_qual = tmp.path().join("max_qual");
        generate_library(&mut rng, &src);

        let source_before = snapshot(&src);
        run_musman(&src, &formats, &max_qual, &[]);
        assert_eq!(
            snapshot(&src),
            source_before,
            "source modified (gen {generation})"
        );

        let (expected_formats, expected_tracks) =
            expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
        assert_tree_matches(&formats, &expected_formats);
        assert_tree_matches(&max_qual, &expected_tracks);
        assert_dirs_match(&formats, &expected_formats);

        // Mutation round: drop about a tenth of the source files and add
        // a brand-new artist, then reconcile again.
        let all_files: Vec<PathBuf> = list_files(&src).values().cloned().collect();
        let mut order: Vec<usize> = (0..all_files.len()).collect();
        order.shuffle(&mut rng);
        for i in order.iter().take(all_files.len() / 10 + 1) {
            fs::remove_file(&all_files[*i]).unwrap();
        }
        let new_artist = format!("Mutated {generation}");
        write_file(
            &src.join(format!("{new_artist}/Solo/01 Song.flac")),
            "new flac",
        );
        write_file(
            &src.join(format!("{new_artist}/Solo/01 Song.mp3")),
            "new mp3",
        );

        run_musman(&src, &formats, &max_qual, &[]);
        let (expected_formats, expected_tracks) =
            expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
        assert_tree_matches(&formats, &expected_formats);
        assert_tree_matches(&max_qual, &expected_tracks);
        assert_dirs_match(&formats, &expected_formats);
    }
}

// ---------------------------------------------------------------------
// Differential test against the legacy script
// ---------------------------------------------------------------------

#[test]
fn matches_legacy_script_on_fresh_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy_src = tmp.path().join("legacy");
    let musman_src = tmp.path().join("musman");
    make_source_tree(&legacy_src);
    make_source_tree(&musman_src);

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("legacy/create_music_links.sh");
    let out = Command::new("bash")
        .arg(&script)
        .args([
            "-f",
            "flac",
            "-f",
            "mp3",
            "-f",
            "ogg",
            "-s",
            legacy_src.display().to_string().as_str(),
            "-m",
            legacy_src.join("max_qual").display().to_string().as_str(),
            "-i",
            legacy_src.join("formats").display().to_string().as_str(),
        ])
        .output()
        .expect("run legacy script");
    assert!(
        out.status.success(),
        "legacy script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    run_musman(
        &musman_src,
        &musman_src.join("formats"),
        &musman_src.join("max_qual"),
        &[],
    );

    let legacy_formats = list_files(&legacy_src.join("formats"));
    let musman_formats = list_files(&musman_src.join("formats"));
    let legacy_rels: BTreeSet<_> = legacy_formats.keys().cloned().collect();
    let musman_rels: BTreeSet<_> = musman_formats.keys().cloned().collect();
    assert_eq!(legacy_rels, musman_rels, "formats trees differ");

    let legacy_tracks = list_files(&legacy_src.join("max_qual"));
    let musman_tracks = list_files(&musman_src.join("max_qual"));
    let legacy_track_rels: BTreeSet<_> = legacy_tracks.keys().cloned().collect();
    let musman_track_rels: BTreeSet<_> = musman_tracks.keys().cloned().collect();
    assert_eq!(
        legacy_track_rels, musman_track_rels,
        "max-qual trees differ"
    );

    let (expected_formats, expected_tracks) = expected_state(
        &musman_src,
        FORMATS,
        &[musman_src.join("formats"), musman_src.join("max_qual")],
    );
    assert_tree_matches(&musman_src.join("formats"), &expected_formats);
    assert_tree_matches(&musman_src.join("max_qual"), &expected_tracks);
}

// ---------------------------------------------------------------------
// Performance smoke test
// ---------------------------------------------------------------------

#[test]
fn performance_on_realistic_library() {
    const ARTISTS: usize = 150;
    const ALBUMS: usize = 10;
    const SONGS: usize = 10;

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    for a in 0..ARTISTS {
        for b in 0..ALBUMS {
            for s in 0..SONGS {
                let base = format!("Artist {a:03}/Album {b:02}/{s:02} Song");
                write_file(&src.join(format!("{base}.flac")), "flac");
                write_file(&src.join(format!("{base}.mp3")), "mp3");
                if a % 3 == 0 {
                    write_file(&src.join(format!("{base}.ogg")), "ogg");
                }
            }
        }
    }

    let formats = tmp.path().join("formats");
    let max_qual = tmp.path().join("max_qual");
    let started = Instant::now();
    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    let elapsed = started.elapsed();

    let songs = ARTISTS * ALBUMS * SONGS;
    let flac_mp3 = songs * 2;
    let ogg = (ARTISTS / 3) * ALBUMS * SONGS;
    let managed = flac_mp3 + ogg;
    let links = managed + songs;
    println!("musman linked {links} files ({songs} songs) in {elapsed:?}");
    assert!(out.contains(&format!("{managed} managed")), "{out}");
    assert!(out.contains(&format!("{songs} tracks")), "{out}");
    assert!(out.contains(&format!("{links} created")), "{out}");
    assert!(
        elapsed < Duration::from_secs(120),
        "smoke bound violated: {elapsed:?}"
    );

    let (out, _) = run_musman(&src, &formats, &max_qual, &[]);
    assert!(out.contains("0 created"), "re-run must be a no-op: {out}");

    let (expected_formats, expected_tracks) =
        expected_state(&src, FORMATS, &[formats.clone(), max_qual.clone()]);
    assert_eq!(expected_formats.len(), managed);
    assert_eq!(expected_tracks.len(), songs);
    assert_tree_matches(&formats, &expected_formats);
    assert_tree_matches(&max_qual, &expected_tracks);
}
