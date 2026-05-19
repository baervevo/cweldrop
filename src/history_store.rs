use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Sent,
    Recv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLine {
    pub dir: Direction,
    pub from: String,
    pub body: String,
    pub at: DateTime<Utc>,
}

pub fn history_root() -> Result<PathBuf> {
    Ok(identity::data_dir()?.join("history"))
}

pub fn path_for(peer_pubkey_hex: &str) -> Result<PathBuf> {
    Ok(history_root()?.join(format!("{peer_pubkey_hex}.jsonl")))
}

pub fn path_for_in(root: &Path, peer_pubkey_hex: &str) -> PathBuf {
    root.join(format!("{peer_pubkey_hex}.jsonl"))
}

pub fn append(peer_pubkey_hex: &str, line: &StoredLine) -> Result<()> {
    let root = history_root()?;
    append_in(&root, peer_pubkey_hex, line)
}

pub fn append_in(root: &Path, peer_pubkey_hex: &str, line: &StoredLine) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let path = path_for_in(root, peer_pubkey_hex);
    let mut f = open_append(&path)?;
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    f.write_all(&bytes)?;
    Ok(())
}

pub fn load(peer_pubkey_hex: &str) -> Result<Vec<StoredLine>> {
    let root = history_root()?;
    load_in(&root, peer_pubkey_hex)
}

pub fn load_in(root: &Path, peer_pubkey_hex: &str) -> Result<Vec<StoredLine>> {
    let path = path_for_in(root, peer_pubkey_hex);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let r = BufReader::new(f);
    let mut out = Vec::new();
    for line in r.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoredLine>(&line) {
            Ok(sl) => out.push(sl),
            Err(e) => tracing::warn!(error=%e, "skip malformed history line"),
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn open_append(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_append(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_root() -> PathBuf {
        let nonce: u64 = rand::random();
        let p = std::env::temp_dir().join(format!("cweldrop-hist-{nonce}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn append_then_load() {
        let root = fresh_root();
        let pk = "aabbcc";
        let l1 = StoredLine {
            dir: Direction::Sent,
            from: "me".into(),
            body: "hi".into(),
            at: Utc::now(),
        };
        let l2 = StoredLine {
            dir: Direction::Recv,
            from: "you".into(),
            body: "hey".into(),
            at: Utc::now(),
        };
        append_in(&root, pk, &l1).unwrap();
        append_in(&root, pk, &l2).unwrap();
        let got = load_in(&root, pk).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "hi");
        assert_eq!(got[1].body, "hey");
        assert!(matches!(got[0].dir, Direction::Sent));
        assert!(matches!(got[1].dir, Direction::Recv));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_missing_returns_empty() {
        let root = fresh_root();
        let got = load_in(&root, "deadbeef").unwrap();
        assert!(got.is_empty());
    }
}
