//! # core::manga
//!
//! Manga subscription store.
//!
//! ## Relationship to `core::subscription`
//! Manga and webnovel subscriptions share one record type and one store
//! implementation ([`crate::core::subscription`]); this module binds it to the
//! `mangas` directory.  A manga chapter differs from a novel chapter only in
//! its payload — page images instead of XHTML — which is handled by
//! `api::manga` and `core::cbz`, not here.
//!
//! ## Storage layout
//! `<vault>/.fero/mangas/<subscription-id>.json`, soft-deleted records
//! under `.../mangas/trash/`.  Page images are cached per chapter inside the
//! novel's vault folder, see `core::cbz`.
//!
//! ## Dependencies
//! - `core::subscription` – record types and store implementation
//! - `core::vault::Vault` – directory resolution (callers pass `system_dir()`)

use std::path::{Path, PathBuf};

use crate::core::subscription;
use crate::error::Result;

pub use crate::core::subscription::{
    blocked_reason, is_valid_subscription_id, normalize_url, subscription_id, unix_now,
    KnownChapter, Subscription,
};

/// Store directory name for manga subscriptions.
const STORE: &str = "mangas";

/// Returns the directory where manga subscription JSON files are stored.
pub fn mangas_dir(system_dir: &Path) -> PathBuf {
    subscription::store_dir(system_dir, STORE)
}

/// Returns the file path for a specific subscription id.
pub fn subscription_file_path(system_dir: &Path, subscription_id: &str) -> PathBuf {
    subscription::subscription_file_path(system_dir, STORE, subscription_id)
}

/// Loads a manga subscription by id, if one exists.
///
/// # Errors
/// - `VaultError::InvalidProperty` if `subscription_id` is not a generated id
pub fn load_subscription(system_dir: &Path, subscription_id: &str) -> Result<Option<Subscription>> {
    subscription::load_subscription(system_dir, STORE, subscription_id)
}

/// Persists a manga subscription, creating the store directory if needed.
///
/// # Errors
/// - `VaultError::InvalidProperty` if the record carries a malformed id
pub fn save_subscription(system_dir: &Path, subscription: &Subscription) -> Result<()> {
    subscription::save_subscription(system_dir, STORE, subscription)
}

/// Directory holding soft-deleted manga subscription records.
pub fn manga_trash_dir(system_dir: &Path) -> PathBuf {
    subscription::trash_dir(system_dir, STORE)
}

/// Moves a manga subscription into the in-app trash (reversible).
///
/// Returns `Ok(None)` when no active subscription with this id exists.
pub fn trash_subscription(
    system_dir: &Path,
    subscription_id: &str,
) -> Result<Option<Subscription>> {
    subscription::trash_subscription(system_dir, STORE, subscription_id)
}

/// Restores a trashed manga subscription back into the active list.
pub fn restore_subscription(system_dir: &Path, subscription_id: &str) -> Result<Subscription> {
    subscription::restore_subscription(system_dir, STORE, subscription_id)
}

/// Lists all trashed manga subscriptions (newest first).
pub fn list_trashed_subscriptions(system_dir: &Path) -> Result<Vec<Subscription>> {
    subscription::list_trashed_subscriptions(system_dir, STORE)
}

/// Permanently removes a trashed manga subscription record.
pub fn purge_trashed_subscription(system_dir: &Path, subscription_id: &str) -> Result<()> {
    subscription::purge_trashed_subscription(system_dir, STORE, subscription_id)
}

/// Deletes a manga subscription record.  The downloaded files are NOT touched
/// — callers decide separately whether to remove the vault folder.
pub fn delete_subscription(system_dir: &Path, subscription_id: &str) -> Result<()> {
    subscription::delete_subscription(system_dir, STORE, subscription_id)
}

/// Returns all manga subscriptions, sorted by creation time (oldest first).
///
/// Files that cannot be parsed are silently skipped so one corrupt record
/// does not hide the rest of the library.
pub fn list_subscriptions(system_dir: &Path) -> Result<Vec<Subscription>> {
    subscription::list_subscriptions(system_dir, STORE)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_system_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("manga-test-{label}-{}", std::process::id()))
    }

    #[test]
    fn store_layout_is_separate_from_webnovels() {
        let dir = Path::new("/vault/.fero");
        assert_eq!(mangas_dir(dir), dir.join("mangas"));
        assert_eq!(manga_trash_dir(dir), dir.join("mangas/trash"));
        assert_ne!(mangas_dir(dir), crate::core::webnovel::webnovels_dir(dir));
    }

    #[test]
    fn save_load_delete_roundtrip() {
        let dir = temp_system_dir("crud");
        let mut subscription = Subscription::new(
            "https://www.mangatown.com/manga/barakamon",
            "mangatown",
            "Barakamon",
        );
        subscription.known_chapters.push(KnownChapter {
            index: 1,
            title: "Chapter 1".to_string(),
            url: "https://www.mangatown.com/manga/barakamon/v01/c001/".to_string(),
            volume: Some("v01".to_string()),
            page_count: Some(42),
            downloaded_at_unix: Some(1_700_000_000),
            placeholder: false,
        });

        save_subscription(&dir, &subscription).expect("save should succeed");
        let loaded = load_subscription(&dir, &subscription.id)
            .expect("load should succeed")
            .expect("subscription should exist");
        assert_eq!(loaded.title, "Barakamon");
        assert_eq!(loaded.known_chapters[0].volume.as_deref(), Some("v01"));
        assert_eq!(loaded.known_chapters[0].page_count, Some(42));

        delete_subscription(&dir, &subscription.id).expect("delete should succeed");
        assert!(load_subscription(&dir, &subscription.id)
            .expect("load should succeed")
            .is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
