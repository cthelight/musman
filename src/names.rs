//! File-name semantics shared across musman.
//!
//! A *track* is the identity of one piece of music across formats: the
//! relative path of a file with its final extension stripped. Two files
//! belong to the same track when their track keys are equal, e.g.
//! `album/01 Song.flac` and `album/01 Song.mp3` are both track
//! `album/01 Song`.

use std::path::Path;

/// Split a file name into its stem and extension.
///
/// The extension is everything after the last dot of the file name (the
/// path is irrelevant): `"1. Song.flac"` -> `("1. Song", "flac")` and
/// `"archive.tar.gz"` -> `("archive.tar", "gz")`.
///
/// Returns `None` for names without a usable extension: no dot at all
/// (`"README"`), a leading dot (`".hidden"`) or a trailing dot
/// (`"weird."`).
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let idx = name.rfind('.')?;
    if idx == 0 || idx + 1 == name.len() {
        return None;
    }
    Some((&name[..idx], &name[idx + 1..]))
}

/// Build the track key for a file: its relative directory (when present)
/// joined with the extension-less file name.
pub fn track_key(rel_dir: &Path, stem: &str) -> String {
    let dir = rel_dir.display().to_string();
    if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_extension() {
        assert_eq!(split_name("01 Song.flac"), Some(("01 Song", "flac")));
        assert_eq!(split_name("track.mp3"), Some(("track", "mp3")));
    }

    #[test]
    fn dotted_track_title() {
        assert_eq!(split_name("1. Song.flac"), Some(("1. Song", "flac")));
    }

    #[test]
    fn multiple_dots_use_the_last_one() {
        assert_eq!(split_name("archive.tar.gz"), Some(("archive.tar", "gz")));
        assert_eq!(split_name("a.b.c.d"), Some(("a.b.c", "d")));
    }

    #[test]
    fn names_without_dot_have_no_extension() {
        assert_eq!(split_name("README"), None);
        assert_eq!(split_name("a"), None);
    }

    #[test]
    fn leading_dot_has_no_extension() {
        assert_eq!(split_name(".hidden"), None);
        assert_eq!(split_name(".flac"), None);
    }

    #[test]
    fn trailing_dot_has_no_extension() {
        assert_eq!(split_name("weird."), None);
    }

    #[test]
    fn track_key_at_source_root() {
        assert_eq!(track_key(Path::new(""), "01 Song"), "01 Song");
    }

    #[test]
    fn track_key_in_subdirectory() {
        assert_eq!(
            track_key(Path::new("album/deep"), "01 Song"),
            "album/deep/01 Song"
        );
    }
}
