//! # core::progress
//!
//! Persistent playback and reading position store.
//!
//! ## Storage layout
//! Each item's progress is stored as a single JSON file under
//! `<vault>/.fero/progress/<key>.json`, where `<key>` is a hex-encoded
//! FNV-1a-64 hash of the vault-relative path string.  Using a path hash keeps
//! file-system names short and filesystem-safe while still being stable as long
//! as the file hasn't moved inside the vault.
//!
//! ## Dependencies
//! - `core::vault::Vault` – directory resolution

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

// ---------------------------------------------------------------------------
// Progress data types
// ---------------------------------------------------------------------------

/// Playback or reading position for one media item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaProgress {
    /// Video file (MP4, MOV, WebM, …).
    Video {
        position_seconds: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_seconds: Option<f64>,
    },
    /// Audio file (MP3, M4A, FLAC, …) or standalone audio track.
    Audio {
        position_seconds: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_seconds: Option<f64>,
    },
    /// Audiobook — may span multiple files, position is in the current part.
    Audiobook {
        part_index: u32,
        position_seconds: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        part_hash: Option<String>,
    },
    /// EPUB file — position encoded as an EPUB CFI string.
    Epub {
        /// EPUB Canonical Fragment Identifier, layout-independent.
        cfi: String,
        /// 0.0 – 1.0 approximate progress for display.
        percentage: f64,
    },
    /// PDF file — page number (1-indexed) and fractional scroll within that page.
    Pdf {
        page: u32,
        /// 0.0 = top of page, 1.0 = bottom.
        scroll_fraction: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_pages: Option<u32>,
    },
    /// Manga or comic — tracked by page index (0-indexed).
    Manga {
        page: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_pages: Option<u32>,
    },
}

impl MediaProgress {
    /// Returns a value in [0.0, 1.0] suitable for a progress bar, if derivable.
    pub fn fraction(&self) -> Option<f64> {
        match self {
            Self::Video {
                position_seconds,
                duration_seconds: Some(dur),
            }
            | Self::Audio {
                position_seconds,
                duration_seconds: Some(dur),
            } => {
                if *dur > 0.0 {
                    Some((position_seconds / dur).clamp(0.0, 1.0))
                } else {
                    None
                }
            }
            Self::Epub { percentage, .. } => Some(percentage.clamp(0.0, 1.0)),
            Self::Pdf {
                page,
                total_pages: Some(total),
                ..
            } => {
                if *total > 0 {
                    Some((*page as f64 / *total as f64).clamp(0.0, 1.0))
                } else {
                    None
                }
            }
            Self::Manga {
                page,
                total_pages: Some(total),
            } => {
                if *total > 0 {
                    Some((*page as f64 / *total as f64).clamp(0.0, 1.0))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Returns `true` if the item appears to be finished (≥ 90 %).
    pub fn is_completed(&self) -> bool {
        self.fraction().map(|f| f >= 0.90).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// On-disk record
// ---------------------------------------------------------------------------

/// The full progress record written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressRecord {
    /// Vault-relative path of the media file this record belongs to.
    pub vault_path: String,
    /// Actual playback / reading position.
    pub progress: MediaProgress,
    /// UNIX timestamp of the last access (seconds since epoch).
    pub last_accessed: u64,
    /// Whether the user has explicitly marked this as completed.
    #[serde(default)]
    pub completed: bool,
}

impl ProgressRecord {
    /// Creates a new record with the current timestamp.
    pub fn new(vault_path: impl Into<String>, progress: MediaProgress) -> Self {
        Self {
            vault_path: vault_path.into(),
            progress,
            last_accessed: unix_now(),
            completed: false,
        }
    }

    /// Returns the approximate progress fraction for UI display.
    pub fn fraction(&self) -> Option<f64> {
        self.progress.fraction()
    }
}

// ---------------------------------------------------------------------------
// Store operations
// ---------------------------------------------------------------------------

/// Derives the progress file path for a given vault-relative path.
///
/// Uses an FNV-1a-64 hash of the path string so the result is filesystem-safe
/// regardless of special characters in the path.
pub fn progress_file_path(progress_dir: &std::path::Path, vault_path: &str) -> PathBuf {
    let key = fnv1a64(vault_path.as_bytes());
    progress_dir.join(format!("{key:016x}.json"))
}

/// Loads the progress record for a vault-relative path, if one exists.
pub fn load_progress(
    progress_dir: &std::path::Path,
    vault_path: &str,
) -> Result<Option<ProgressRecord>> {
    let file_path = progress_file_path(progress_dir, vault_path);

    if !file_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&file_path).map_err(VaultError::from)?;
    let record: ProgressRecord = serde_json::from_str(&raw)
        .map_err(|e| VaultError::InvalidVaultPath(format!("progress JSON parse error: {e}")))?;

    Ok(Some(record))
}

/// Persists a progress record, creating the progress directory if needed.
pub fn save_progress(
    progress_dir: &std::path::Path,
    vault_path: &str,
    progress: MediaProgress,
    completed: bool,
) -> Result<()> {
    fs::create_dir_all(progress_dir).map_err(VaultError::from)?;

    let mut record = ProgressRecord::new(vault_path, progress);
    record.completed = completed;

    let file_path = progress_file_path(progress_dir, vault_path);
    let json = serde_json::to_string_pretty(&record)
        .map_err(|e| VaultError::InvalidVaultPath(format!("progress JSON serialize error: {e}")))?;

    fs::write(&file_path, json).map_err(VaultError::from)?;
    Ok(())
}

/// Deletes the progress record for a vault-relative path (marks as not started).
pub fn delete_progress(progress_dir: &std::path::Path, vault_path: &str) -> Result<()> {
    let file_path = progress_file_path(progress_dir, vault_path);
    if file_path.exists() {
        fs::remove_file(&file_path).map_err(VaultError::from)?;
    }
    Ok(())
}

/// Returns all progress records, sorted by most recently accessed first.
///
/// Items that cannot be parsed are silently skipped.
pub fn list_in_progress(progress_dir: &std::path::Path) -> Result<Vec<ProgressRecord>> {
    if !progress_dir.exists() {
        return Ok(Vec::new());
    }

    let mut records: Vec<ProgressRecord> = fs::read_dir(progress_dir)
        .map_err(VaultError::from)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let raw = fs::read_to_string(&path).ok()?;
            let record: ProgressRecord = serde_json::from_str(&raw).ok()?;
            if record.completed {
                return None;
            }
            Some(record)
        })
        .collect();

    records.sort_by_key(|record| std::cmp::Reverse(record.last_accessed));
    Ok(records)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// FNV-1a 64-bit hash — matches the implementation in `core::duplicate`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut state = OFFSET;
    for &b in bytes {
        state ^= u64::from(b);
        state = state.wrapping_mul(PRIME);
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_progress_fraction() {
        let p = MediaProgress::Video {
            position_seconds: 900.0,
            duration_seconds: Some(3600.0),
        };
        assert!((p.fraction().unwrap() - 0.25).abs() < 1e-9);
        assert!(!p.is_completed());
    }

    #[test]
    fn video_progress_completed() {
        let p = MediaProgress::Video {
            position_seconds: 3300.0,
            duration_seconds: Some(3600.0),
        };
        assert!(p.is_completed());
    }

    #[test]
    fn epub_progress_fraction() {
        let p = MediaProgress::Epub {
            cfi: "epubcfi(/6/4!/4)".to_string(),
            percentage: 0.55,
        };
        assert!((p.fraction().unwrap() - 0.55).abs() < 1e-9);
    }

    #[test]
    fn progress_file_path_is_deterministic() {
        use std::path::Path;
        let dir = Path::new("/vault/.fero/progress");
        let a = progress_file_path(dir, "Anime/Test.mkv");
        let b = progress_file_path(dir, "Anime/Test.mkv");
        assert_eq!(a, b);
    }

    #[test]
    fn different_paths_get_different_keys() {
        use std::path::Path;
        let dir = Path::new("/vault/.fero/progress");
        let a = progress_file_path(dir, "Anime/A.mkv");
        let b = progress_file_path(dir, "Filme/A.mkv");
        assert_ne!(a, b);
    }
}
