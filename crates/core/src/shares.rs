//! Shared folders and safe access to what is inside them.
//!
//! Peers name files with `/`-separated segments relative to a share. Nothing
//! outside a shared folder is ever served: `.`/`..` segments are refused and
//! the canonical path (symlinks resolved) must stay inside the share.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::protocol::Entry;

#[derive(Clone, Debug, PartialEq)]
pub struct Share {
    /// Name the others see (the folder's name, made unique).
    pub name: String,
    pub path: PathBuf,
}

/// Shares for these folders, named after the folders: "Music", "Music (2)", …
pub fn from_paths(paths: &[PathBuf]) -> Vec<Share> {
    let mut out: Vec<Share> = Vec::new();
    for path in paths {
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| is_safe_name(n))
            .unwrap_or_else(|| "Files".into());
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|s| s.name == name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        out.push(Share {
            name,
            path: path.clone(),
        });
    }
    out
}

/// A single file or folder name that can be used as-is on disk.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// The share containing `path` and the segments inside it, if any.  Paths
/// are compared after resolving links, as the server resolves them.
pub fn locate(shares: &[Share], path: &Path) -> Option<(String, Vec<String>)> {
    let real = path.canonicalize().ok()?;
    for share in shares {
        let Ok(root) = share.path.canonicalize() else {
            continue;
        };
        let Ok(rel) = real.strip_prefix(&root) else {
            continue;
        };
        let segments: Option<Vec<String>> = rel
            .components()
            .map(|c| c.as_os_str().to_str().map(str::to_string))
            .collect();
        if let Some(segments) = segments.filter(|s| !s.is_empty()) {
            return Some((share.name.clone(), segments));
        }
    }
    None
}

#[derive(Debug, PartialEq)]
pub enum Denied {
    NotFound,
    Forbidden,
}

/// The real path of `segments` inside `root`.
pub fn resolve(root: &Path, segments: &[String]) -> Result<PathBuf, Denied> {
    let root = root.canonicalize().map_err(|_| Denied::NotFound)?;
    let mut path = root.clone();
    for s in segments {
        if !is_safe_name(s) {
            return Err(Denied::Forbidden);
        }
        path.push(s);
    }
    let real = path.canonicalize().map_err(|_| Denied::NotFound)?;
    if real.starts_with(&root) {
        Ok(real)
    } else {
        Err(Denied::Forbidden)
    }
}

/// Folder contents: folders first, then by name (case-insensitive).
/// Entries that lead outside `root`, broken links, sockets etc. are left out.
pub fn list_dir(root: &Path, dir: &Path) -> io::Result<Vec<Entry>> {
    let root = root.canonicalize()?;
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        let path = item.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if item.file_type().is_ok_and(|t| t.is_symlink())
            && !path.canonicalize().is_ok_and(|p| p.starts_with(&root))
        {
            continue;
        }
        if !meta.is_dir() && !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        out.push(Entry {
            name,
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified,
        });
    }
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn segs(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_are_unique() {
        let shares = from_paths(&[
            "/a/Music".into(),
            "/b/Music".into(),
            "/".into(),
            "/c/Music".into(),
        ]);
        let names: Vec<_> = shares.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Music", "Music (2)", "Files", "Music (3)"]);
    }

    #[test]
    fn locate_in_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("Music");
        fs::create_dir_all(music.join("Album")).unwrap();
        fs::write(music.join("Album/a.mp3"), "x").unwrap();
        fs::write(tmp.path().join("other.txt"), "x").unwrap();
        symlink(music.join("Album"), tmp.path().join("link")).unwrap();
        let shares = from_paths(std::slice::from_ref(&music));
        let want = Some(("Music".to_string(), vec!["Album".into(), "a.mp3".into()]));
        assert_eq!(locate(&shares, &music.join("Album/a.mp3")), want);
        assert_eq!(locate(&shares, &tmp.path().join("link/a.mp3")), want);
        // The share itself, things outside it, and missing files are not.
        assert_eq!(locate(&shares, &music), None);
        assert_eq!(locate(&shares, &tmp.path().join("other.txt")), None);
        assert_eq!(locate(&shares, &music.join("nope")), None);
    }

    #[test]
    fn safe_names() {
        assert!(is_safe_name("a b.txt"));
        assert!(is_safe_name("..hidden"));
        for bad in ["", ".", "..", "a/b", "/", "a\0"] {
            assert!(!is_safe_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn resolve_stays_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("share");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/f.txt"), "x").unwrap();
        fs::write(tmp.path().join("secret"), "s").unwrap();
        symlink(tmp.path().join("secret"), root.join("escape")).unwrap();
        symlink(root.join("sub/f.txt"), root.join("inside")).unwrap();

        let real_root = root.canonicalize().unwrap();
        assert_eq!(resolve(&root, &[]), Ok(real_root.clone()));
        assert_eq!(
            resolve(&root, &segs(&["sub", "f.txt"])),
            Ok(real_root.join("sub/f.txt"))
        );
        assert_eq!(
            resolve(&root, &segs(&["inside"])),
            Ok(real_root.join("sub/f.txt"))
        );
        assert_eq!(resolve(&root, &segs(&[".."])), Err(Denied::Forbidden));
        assert_eq!(
            resolve(&root, &segs(&["sub", "..", ".."])),
            Err(Denied::Forbidden)
        );
        assert_eq!(
            resolve(&root, &segs(&["a/../../secret"])),
            Err(Denied::Forbidden)
        );
        assert_eq!(resolve(&root, &segs(&["escape"])), Err(Denied::Forbidden));
        assert_eq!(resolve(&root, &segs(&["nope"])), Err(Denied::NotFound));
        assert_eq!(
            resolve(&tmp.path().join("gone"), &[]),
            Err(Denied::NotFound)
        );
    }

    #[test]
    fn listing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("share");
        fs::create_dir_all(root.join("b-dir")).unwrap();
        fs::create_dir_all(root.join("A-dir")).unwrap();
        fs::write(root.join("z.txt"), "12345").unwrap();
        fs::write(root.join("B.txt"), "").unwrap();
        fs::write(tmp.path().join("outside"), "").unwrap();
        symlink(tmp.path().join("outside"), root.join("escape")).unwrap();
        symlink(root.join("missing"), root.join("broken")).unwrap();

        let list = list_dir(&root, &root).unwrap();
        let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A-dir", "b-dir", "B.txt", "z.txt"]);
        assert!(list[0].dir && !list[3].dir);
        assert_eq!(list[3].size, 5);
        assert!(list[3].modified > 1_600_000_000);
    }
}
