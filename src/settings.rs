//! User settings, editable at runtime from the `:settings` panel and persisted
//! as JSON next to the identity (`~/.local/share/cweldrop/settings.json`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity;

const FILE: &str = "settings.json";

/// Choices offered for the incoming file size cap, in MiB.
pub const MAX_FILE_MB_CHOICES: &[u64] = &[10, 50, 100, 500, 1024, 4096];

/// What to do when a peer offers to send a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePolicy {
    /// Show the offer and wait for `:accept` / `:reject`.
    Ask,
    /// Save files from connected peers without asking.
    AutoAccept,
    /// Decline every offer.
    Reject,
}

impl FilePolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoAccept => "auto-accept",
            Self::Reject => "reject",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AutoAccept,
            Self::AutoAccept => Self::Reject,
            Self::Reject => Self::Ask,
        }
    }

    pub fn prev(self) -> Self {
        self.next().next()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub incoming_files: FilePolicy,
    pub max_file_mb: u64,
    pub download_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        let base = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            incoming_files: FilePolicy::Ask,
            max_file_mb: 100,
            download_dir: base.join("cweldrop"),
        }
    }
}

impl Settings {
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1024 * 1024)
    }

    /// Step the size cap to the next (`up`) or previous preset.
    pub fn cycle_max_file_mb(&mut self, up: bool) {
        let c = MAX_FILE_MB_CHOICES;
        let pos = c
            .iter()
            .position(|&v| v >= self.max_file_mb)
            .unwrap_or(c.len() - 1);
        let pos = if up {
            (pos + 1) % c.len()
        } else {
            (pos + c.len() - 1) % c.len()
        };
        self.max_file_mb = c[pos];
    }

    /// Load from the real data dir, falling back to defaults.
    pub fn load() -> Self {
        match identity::data_dir() {
            Ok(dir) => Self::load_in(&dir),
            Err(_) => Self::default(),
        }
    }

    pub fn load_in(dir: &Path) -> Self {
        let path = dir.join(FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "bad settings, using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_in(&identity::data_dir()?)
    }

    pub fn save_in(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(FILE);
        let tmp = dir.join(format!("{FILE}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("cweldrop-settings-{}", rand::random::<u64>()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tmp_dir();
        assert_eq!(Settings::load_in(&dir), Settings::default());
        let s = Settings {
            incoming_files: FilePolicy::AutoAccept,
            max_file_mb: 500,
            download_dir: "/tmp/x".into(),
        };
        s.save_in(&dir).unwrap();
        assert_eq!(Settings::load_in(&dir), s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fields_take_defaults() {
        let dir = tmp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE), r#"{"incoming_files":"reject"}"#).unwrap();
        let s = Settings::load_in(&dir);
        assert_eq!(s.incoming_files, FilePolicy::Reject);
        assert_eq!(s.max_file_mb, Settings::default().max_file_mb);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cycles_wrap() {
        assert_eq!(FilePolicy::Ask.next().next().next(), FilePolicy::Ask);
        assert_eq!(FilePolicy::Ask.prev(), FilePolicy::Reject);
        let mut s = Settings {
            max_file_mb: 4096,
            ..Settings::default()
        };
        s.cycle_max_file_mb(true);
        assert_eq!(s.max_file_mb, 10);
        s.cycle_max_file_mb(false);
        assert_eq!(s.max_file_mb, 4096);
    }
}
