//! fzf-style file search for `:send`: index a folder in the background,
//! then fuzzy-match typed text against the relative paths.

use std::path::{Path, PathBuf};

use crate::commands::fuzzy;

/// Stop indexing after this many files (keeps huge trees responsive).
pub const MAX_FILES: usize = 20_000;
/// How deep below the base folder to look.
pub const MAX_DEPTH: usize = 8;
/// Directory names never descended into.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "__pycache__"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIndex {
    pub root: PathBuf,
    /// Paths relative to `root`, `/`-separated.
    pub paths: Vec<String>,
    /// Hit `MAX_FILES` before finishing.
    pub truncated: bool,
}

/// Walk `root` (no hidden or build folders, symlinked dirs not followed).
pub fn index(root: &Path) -> FileIndex {
    let mut paths = Vec::new();
    let mut truncated = false;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let is_file = ft.is_file() || (ft.is_symlink() && path.is_file());
            if !is_file {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                paths.push(rel.to_string_lossy().replace('\\', "/"));
            }
            if paths.len() >= MAX_FILES {
                truncated = true;
                break 'walk;
            }
        }
    }
    paths.sort();
    FileIndex {
        root: root.to_path_buf(),
        paths,
        truncated,
    }
}

/// How a `:send` argument splits into a folder to search and a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Folder to index.
    pub base: PathBuf,
    /// The typed folder part, kept verbatim for completion (`~/Pictures/`).
    pub prefix: String,
    pub query: String,
}

/// `~/Pictures/ca` → search `~/Pictures` for "ca"; `cat` → search `cwd`.
pub fn scope(arg: &str, cwd: &Path) -> Scope {
    if let Some(idx) = arg.rfind('/') {
        let (dir, rest) = arg.split_at(idx + 1);
        let expanded = crate::files::expand_tilde(dir);
        let base = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };
        if base.is_dir() {
            return Scope {
                base,
                prefix: dir.to_string(),
                query: rest.to_string(),
            };
        }
    }
    Scope {
        base: cwd.to_path_buf(),
        prefix: String::new(),
        query: arg.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Relative to the index root.
    pub path: String,
    pub score: i32,
    /// Matched char indices in `path`.
    pub hits: Vec<usize>,
}

/// Best `limit` matches for `query`, favouring matches in the file name and
/// shorter paths. An empty query lists the first files.
pub fn search(index: &FileIndex, query: &str, limit: usize) -> Vec<Hit> {
    let q: String = query.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out: Vec<Hit> = index
        .paths
        .iter()
        .filter_map(|p| {
            let (mut score, hits) = fuzzy(&q, p)?;
            let name_start = p.rfind('/').map_or(0, |i| i + 1);
            let name_chars = p[..name_start].chars().count();
            if !q.is_empty() && hits.iter().all(|&h| h >= name_chars) {
                score += 15; // whole query inside the file name
            }
            score -= (p.len() / 16) as i32;
            Some(Hit {
                path: p.clone(),
                score,
                hits,
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.len().cmp(&b.path.len())));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cweldrop-fs-{}", rand::random::<u64>()));
        for f in [
            "photos/cat.png",
            "photos/2024/beach.jpg",
            "docs/report.pdf",
            "docs/notes.txt",
            "category/readme.md",
            ".git/config",
            "target/debug/big.bin",
            ".hidden.txt",
        ] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        root
    }

    #[test]
    fn index_skips_hidden_and_build_dirs() {
        let root = tree();
        let idx = index(&root);
        assert_eq!(
            idx.paths,
            vec![
                "category/readme.md",
                "docs/notes.txt",
                "docs/report.pdf",
                "photos/2024/beach.jpg",
                "photos/cat.png"
            ]
        );
        assert!(!idx.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_prefers_file_name_matches() {
        let root = tree();
        let idx = index(&root);
        let top = |q: &str| search(&idx, q, 5).first().map(|h| h.path.clone());
        assert_eq!(
            top("cat").as_deref(),
            Some("photos/cat.png"),
            "name beats dir 'category'"
        );
        assert_eq!(top("rpt").as_deref(), Some("docs/report.pdf"));
        assert_eq!(top("bch").as_deref(), Some("photos/2024/beach.jpg"));
        assert_eq!(
            top("ph cat").as_deref(),
            Some("photos/cat.png"),
            "spaces ignored"
        );
        assert!(search(&idx, "zzz", 5).is_empty());
        assert_eq!(search(&idx, "", 3).len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_splits_folder_and_query() {
        let root = tree();
        let s = scope("photos/ca", &root);
        assert_eq!(s.base, root.join("photos/"));
        assert_eq!((s.prefix.as_str(), s.query.as_str()), ("photos/", "ca"));
        let s = scope("cat", &root);
        assert_eq!((s.base.clone(), s.query.as_str()), (root.clone(), "cat"));
        let abs = format!("{}/docs/no", root.display());
        let s = scope(&abs, Path::new("/"));
        assert_eq!(s.base, root.join("docs/"));
        assert_eq!(s.query, "no");
        // Not a folder: whole thing is the query in cwd.
        let s = scope("nope/x", &root);
        assert_eq!((s.base, s.query.as_str()), (root.clone(), "nope/x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn index_caps_file_count() {
        let root = std::env::temp_dir().join(format!("cweldrop-fs-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..(MAX_FILES + 5) {
            std::fs::write(root.join(format!("f{i}")), b"").unwrap();
        }
        let idx = index(&root);
        assert_eq!(idx.paths.len(), MAX_FILES);
        assert!(idx.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }
}
