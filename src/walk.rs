//! Parallel directory traversal producing owned entries.
//!
//! Classification relies on the `d_type` field that `getdents` returns on
//! Linux: when the filesystem (including most NFS servers) fills it in,
//! telling a file from a directory costs no extra syscall. Only entries
//! with an unknown type fall back to `lstat`, which matters a lot on
//! slow network mounts where every syscall is a round trip.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// What an entry is, from the walker's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Regular file.
    File,
    /// Directory (walked recursively).
    Dir,
    /// Symlink, device, socket, fifo, or anything else.
    Other,
}

/// An owned snapshot of one directory entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Absolute path of the entry.
    pub path: PathBuf,
    /// The entry's type.
    pub kind: Kind,
}

/// Recursively walk `root` in parallel (breadth-first) and return every
/// entry, including `root` itself.
///
/// Semantics mirror plain `find`: hidden entries are included, no
/// ignore rules apply, and symlinks are not followed. IO errors are
/// returned as `Err` items (their order is not guaranteed).
pub fn walk_all(root: &Path) -> Vec<Result<Entry, io::Error>> {
    let mut out: Vec<Result<Entry, io::Error>> = Vec::new();
    out.push(Ok(Entry {
        path: root.to_path_buf(),
        kind: Kind::Dir,
    }));

    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    while !level.is_empty() {
        let scans: Vec<DirRead> = level.par_iter().map(|dir| read_dir_entries(dir)).collect();
        let mut next = Vec::new();
        for (entries, subdirs) in scans {
            next.extend(subdirs);
            out.extend(entries);
        }
        level = next;
    }
    out
}

/// One directory's read: its classified entries plus the subdirectories
/// to walk in the next level.
type DirRead = (Vec<Result<Entry, io::Error>>, Vec<PathBuf>);

/// Read one directory: classify its entries and collect the
/// subdirectories to walk next.
fn read_dir_entries(dir: &Path) -> DirRead {
    let mut entries: Vec<Result<Entry, io::Error>> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();

    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) => {
            entries.push(Err(io::Error::new(
                err.kind(),
                format!("read directory {}: {err}", dir.display()),
            )));
            return (entries, subdirs);
        }
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                entries.push(Err(io::Error::new(
                    err.kind(),
                    format!("read directory entry in {}: {err}", dir.display()),
                )));
                continue;
            }
        };
        let path = entry.path();
        let kind = match entry.file_type() {
            // Uses d_type from the readdir result when available.
            Ok(ft) if ft.is_dir() => {
                subdirs.push(path.clone());
                Kind::Dir
            }
            Ok(ft) if ft.is_file() => Kind::File,
            Ok(_) => Kind::Other,
            Err(err) => {
                entries.push(Err(io::Error::new(
                    err.kind(),
                    format!("stat {}: {err}", path.display()),
                )));
                continue;
            }
        };
        entries.push(Ok(Entry { path, kind }));
    }
    (entries, subdirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_everything_including_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".hid/sub")).unwrap();
        std::fs::write(root.join("a.bin"), "a").unwrap();
        std::fs::write(root.join(".hid/sub/b.bin"), "b").unwrap();
        std::os::unix::fs::symlink(root.join("a.bin"), root.join("link.bin")).unwrap();

        let entries = walk_all(root);
        let by_path: std::collections::HashMap<PathBuf, Kind> = entries
            .into_iter()
            .map(|e| {
                let e = e.unwrap();
                (e.path, e.kind)
            })
            .collect();

        assert_eq!(by_path.get(&root.to_path_buf()), Some(&Kind::Dir));
        assert_eq!(by_path.get(&root.join("a.bin")), Some(&Kind::File));
        assert_eq!(by_path.get(&root.join(".hid")), Some(&Kind::Dir));
        assert_eq!(by_path.get(&root.join(".hid/sub/b.bin")), Some(&Kind::File));
        assert_eq!(by_path.get(&root.join("link.bin")), Some(&Kind::Other));
    }
}
