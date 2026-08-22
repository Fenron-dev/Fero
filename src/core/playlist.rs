//! # core::playlist
//!
//! Playlist definitions, smart-playlist filters, and playback cursor tracking.
//!
//! ## Storage layout
//! Each playlist is stored as a single YAML file under
//! `<vault>/.fero/playlists/<id>.yaml`.  Smart playlists store their
//! filter rules in the same file; the actual item list is derived at runtime.
//!
//! ## Smart playlist filters
//! Items are matched against `PlaylistFilter` rules applied to the Properties
//! stored in sidecar YAML files.  Sorting uses the same property keys.
//!
//! ## Playback cursor
//! Resume state for a playlist is stored separately in the progress store
//! under `.fero/progress/playlist_<id>.json` so that it shares the same
//! cleanup semantics as per-file progress records.
//!
//! ## Dependencies
//! - `core::vault::Vault` – directory resolution

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

/// A single sort key used by smart playlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortRule {
    /// Property field name (e.g. `series_title`, `volume_number`, `year`).
    pub field: String,
    #[serde(default)]
    pub direction: SortDir,
}

/// Conditions that a media item must satisfy to appear in a smart playlist.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaylistFilter {
    /// If set, only items of this media type are included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// All listed tags must be present on the item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_includes: Vec<String>,
    /// None of the listed tags may be present on the item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_excludes: Vec<String>,
    /// If set, series_title must equal this value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_title: Option<String>,
    /// If set, only items with status equal to this value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// The type of playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PlaylistKind {
    /// Manually ordered list of vault-relative paths.
    #[default]
    Manual,
    /// Dynamically generated from filter + sort rules.
    Smart,
    /// Consecutive episodes of one series (auto-generated).
    Series,
}

/// A playlist definition as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    /// Stable identifier (URL-safe slug or UUID).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    #[serde(default)]
    pub kind: PlaylistKind,
    /// For `Manual` playlists: vault-relative paths in playback order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// For `Smart`/`Series` playlists: filter conditions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<PlaylistFilter>,
    /// For `Smart`/`Series` playlists: sort rules applied after filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortRule>,
    /// Creation timestamp (UNIX seconds).
    #[serde(default)]
    pub created_at: u64,
    /// Last modification timestamp (UNIX seconds).
    #[serde(default)]
    pub updated_at: u64,
}

impl Playlist {
    /// Creates a new manual playlist with the given id and name.
    pub fn new_manual(id: impl Into<String>, name: impl Into<String>) -> Self {
        let now = unix_now();
        Self {
            id: id.into(),
            name: name.into(),
            kind: PlaylistKind::Manual,
            items: Vec::new(),
            filter: None,
            sort: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Creates a new smart playlist with the given filter and sort rules.
    pub fn new_smart(
        id: impl Into<String>,
        name: impl Into<String>,
        filter: PlaylistFilter,
        sort: Vec<SortRule>,
    ) -> Self {
        let now = unix_now();
        Self {
            id: id.into(),
            name: name.into(),
            kind: PlaylistKind::Smart,
            items: Vec::new(),
            filter: Some(filter),
            sort,
            created_at: now,
            updated_at: now,
        }
    }
}

/// The playback cursor for a playlist — remembers where the user left off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistCursor {
    /// Playlist id.
    pub playlist_id: String,
    /// 0-based index of the currently active item.
    pub index: u32,
    /// Vault-relative path of the active file (for integrity check).
    pub vault_path: String,
    /// Playback position in the active file (seconds).
    pub position_seconds: f64,
    /// UNIX timestamp of the last update.
    pub last_accessed: u64,
}

impl PlaylistCursor {
    /// Creates a new cursor at the beginning of a playlist.
    pub fn new(playlist_id: impl Into<String>, first_path: impl Into<String>) -> Self {
        Self {
            playlist_id: playlist_id.into(),
            index: 0,
            vault_path: first_path.into(),
            position_seconds: 0.0,
            last_accessed: unix_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Store operations
// ---------------------------------------------------------------------------

/// Returns the directory where playlist YAML files are stored.
pub fn playlists_dir(system_dir: &Path) -> PathBuf {
    system_dir.join("playlists")
}

/// Returns the file path for a specific playlist.
pub fn playlist_file_path(system_dir: &Path, playlist_id: &str) -> PathBuf {
    playlists_dir(system_dir).join(format!("{}.json", sanitize_id(playlist_id)))
}

/// Loads a playlist by id.  Playlists are stored as JSON (`.json`) files
/// despite the `.yaml`-named constant — using JSON avoids an extra dependency.
pub fn load_playlist(system_dir: &Path, playlist_id: &str) -> Result<Option<Playlist>> {
    let path = playlist_file_path(system_dir, playlist_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(VaultError::from)?;
    let playlist: Playlist = serde_json::from_str(&raw)
        .map_err(|e| VaultError::InvalidVaultPath(format!("playlist JSON parse error: {e}")))?;
    Ok(Some(playlist))
}

/// Saves a playlist to disk.
pub fn save_playlist(system_dir: &Path, playlist: &mut Playlist) -> Result<()> {
    let dir = playlists_dir(system_dir);
    fs::create_dir_all(&dir).map_err(VaultError::from)?;
    playlist.updated_at = unix_now();
    let path = playlist_file_path(system_dir, &playlist.id);
    let json = serde_json::to_string_pretty(playlist)
        .map_err(|e| VaultError::InvalidVaultPath(format!("playlist JSON serialize error: {e}")))?;
    fs::write(&path, json).map_err(VaultError::from)?;
    Ok(())
}

/// Deletes a playlist by id.
pub fn delete_playlist(system_dir: &Path, playlist_id: &str) -> Result<()> {
    let path = playlist_file_path(system_dir, playlist_id);
    if path.exists() {
        fs::remove_file(&path).map_err(VaultError::from)?;
    }
    Ok(())
}

/// Returns all stored playlists, sorted by name.
pub fn list_playlists(system_dir: &Path) -> Result<Vec<Playlist>> {
    let dir = playlists_dir(system_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut playlists: Vec<Playlist> = fs::read_dir(&dir)
        .map_err(VaultError::from)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let raw = fs::read_to_string(&path).ok()?;
            serde_json::from_str::<Playlist>(&raw).ok()
        })
        .collect();
    playlists.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(playlists)
}

// ---------------------------------------------------------------------------
// Cursor helpers
// ---------------------------------------------------------------------------

/// Returns the progress-store path for a playlist cursor.
pub fn cursor_file_path(progress_dir: &Path, playlist_id: &str) -> PathBuf {
    progress_dir.join(format!("playlist_{}.json", sanitize_id(playlist_id)))
}

/// Loads the playback cursor for a playlist.
pub fn load_cursor(progress_dir: &Path, playlist_id: &str) -> Result<Option<PlaylistCursor>> {
    let path = cursor_file_path(progress_dir, playlist_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(VaultError::from)?;
    let cursor: PlaylistCursor = serde_json::from_str(&raw).map_err(|e| {
        VaultError::InvalidVaultPath(format!("playlist cursor JSON parse error: {e}"))
    })?;
    Ok(Some(cursor))
}

/// Saves the playback cursor for a playlist.
pub fn save_cursor(progress_dir: &Path, cursor: &mut PlaylistCursor) -> Result<()> {
    fs::create_dir_all(progress_dir).map_err(VaultError::from)?;
    cursor.last_accessed = unix_now();
    let path = cursor_file_path(progress_dir, &cursor.playlist_id);
    let json = serde_json::to_string_pretty(cursor).map_err(|e| {
        VaultError::InvalidVaultPath(format!("playlist cursor JSON serialize error: {e}"))
    })?;
    fs::write(&path, json).map_err(VaultError::from)?;
    Ok(())
}

/// Deletes the playback cursor for a playlist.
pub fn delete_cursor(progress_dir: &Path, playlist_id: &str) -> Result<()> {
    let path = cursor_file_path(progress_dir, playlist_id);
    if path.exists() {
        fs::remove_file(&path).map_err(VaultError::from)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strips characters that are unsafe in file names, leaving only alphanumeric,
/// hyphen, and underscore.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_replaces_slashes() {
        assert_eq!(sanitize_id("my/playlist"), "my_playlist");
    }

    #[test]
    fn new_manual_playlist_is_empty() {
        let p = Playlist::new_manual("p1", "Test");
        assert_eq!(p.kind, PlaylistKind::Manual);
        assert!(p.items.is_empty());
    }

    #[test]
    fn smart_playlist_has_filter() {
        let filter = PlaylistFilter {
            media_type: Some("audiobook".to_string()),
            tags_includes: vec!["litrpg".to_string()],
            ..Default::default()
        };
        let p = Playlist::new_smart("p2", "LitRPG", filter, vec![]);
        assert_eq!(p.kind, PlaylistKind::Smart);
        assert!(p.filter.is_some());
    }
}
