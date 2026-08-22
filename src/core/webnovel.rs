//! # core::webnovel
//!
//! Webnovel subscription store.
//!
//! ## Relationship to `core::subscription`
//! The record shape, identity rules, trash, and blocklist are shared with the
//! manga engine and live in [`crate::core::subscription`].  This module binds
//! that generic store to the `webnovels` directory so callers keep a
//! self-documenting API (`webnovels_dir`, `webnovel_trash_dir`) and cannot
//! accidentally read a manga record through a webnovel call.
//!
//! ## Storage layout
//! `<vault>/.fero/webnovels/<subscription-id>.json`, soft-deleted
//! records under `.../webnovels/trash/`.
//!
//! ## Dependencies
//! - `core::subscription` – record types and store implementation
//! - `core::vault::Vault` – directory resolution (callers pass `system_dir()`)

use std::path::{Path, PathBuf};

use crate::core::subscription;
use crate::error::Result;

pub use crate::core::subscription::{
    blocked_reason, blocklist_file_path, is_valid_subscription_id, load_blocked_hosts,
    load_blocklist_entries, normalize_url, save_user_blocklist, subscription_id, unix_now,
    BlocklistEntry, KnownChapter, Subscription,
};

/// Store directory name for webnovel subscriptions.
const STORE: &str = "webnovels";

/// Returns the directory where subscription JSON files are stored.
pub fn webnovels_dir(system_dir: &Path) -> PathBuf {
    subscription::store_dir(system_dir, STORE)
}

/// Returns the file path for a specific subscription id.
pub fn subscription_file_path(system_dir: &Path, subscription_id: &str) -> PathBuf {
    subscription::subscription_file_path(system_dir, STORE, subscription_id)
}

/// Loads a subscription by id, if one exists.
///
/// # Errors
/// - `VaultError::InvalidProperty` if `subscription_id` is not a generated id
pub fn load_subscription(system_dir: &Path, subscription_id: &str) -> Result<Option<Subscription>> {
    subscription::load_subscription(system_dir, STORE, subscription_id)
}

/// Persists a subscription, creating the store directory if needed.
///
/// # Errors
/// - `VaultError::InvalidProperty` if the record carries a malformed id
pub fn save_subscription(system_dir: &Path, subscription: &Subscription) -> Result<()> {
    subscription::save_subscription(system_dir, STORE, subscription)
}

/// Directory holding soft-deleted subscription records.
pub fn webnovel_trash_dir(system_dir: &Path) -> PathBuf {
    subscription::trash_dir(system_dir, STORE)
}

/// Moves a subscription record into the in-app trash (reversible).
///
/// Returns `Ok(None)` when no active subscription with this id exists.
pub fn trash_subscription(
    system_dir: &Path,
    subscription_id: &str,
) -> Result<Option<Subscription>> {
    subscription::trash_subscription(system_dir, STORE, subscription_id)
}

/// Restores a trashed subscription back into the active list.
pub fn restore_subscription(system_dir: &Path, subscription_id: &str) -> Result<Subscription> {
    subscription::restore_subscription(system_dir, STORE, subscription_id)
}

/// Lists all trashed subscriptions (newest first).
pub fn list_trashed_subscriptions(system_dir: &Path) -> Result<Vec<Subscription>> {
    subscription::list_trashed_subscriptions(system_dir, STORE)
}

/// Permanently removes a trashed subscription record.
pub fn purge_trashed_subscription(system_dir: &Path, subscription_id: &str) -> Result<()> {
    subscription::purge_trashed_subscription(system_dir, STORE, subscription_id)
}

/// Deletes a subscription record.  The novel's downloaded files are NOT
/// touched — callers decide separately whether to remove the vault folder.
pub fn delete_subscription(system_dir: &Path, subscription_id: &str) -> Result<()> {
    subscription::delete_subscription(system_dir, STORE, subscription_id)
}

/// Returns all subscriptions, sorted by creation time (oldest first).
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
        std::env::temp_dir().join(format!("webnovel-test-{label}-{}", std::process::id()))
    }

    /// The store path must stay exactly where earlier versions wrote it —
    /// a change here would orphan every existing subscription.
    #[test]
    fn store_layout_is_stable() {
        let dir = Path::new("/vault/.fero");
        assert_eq!(webnovels_dir(dir), dir.join("webnovels"));
        assert_eq!(
            subscription_file_path(dir, "0123456789abcdef"),
            dir.join("webnovels/0123456789abcdef.json")
        );
        assert_eq!(webnovel_trash_dir(dir), dir.join("webnovels/trash"));
    }

    #[test]
    fn save_load_trash_restore_roundtrip() {
        let dir = temp_system_dir("roundtrip");
        let subscription =
            Subscription::new("https://example.com/fiction/7", "royalroad", "Test Novel");

        save_subscription(&dir, &subscription).expect("save should succeed");
        assert_eq!(
            list_subscriptions(&dir).expect("list should succeed").len(),
            1
        );

        trash_subscription(&dir, &subscription.id).expect("trash should succeed");
        assert!(list_subscriptions(&dir)
            .expect("list should succeed")
            .is_empty());
        assert_eq!(
            list_trashed_subscriptions(&dir)
                .expect("trash list should succeed")
                .len(),
            1
        );

        restore_subscription(&dir, &subscription.id).expect("restore should succeed");
        assert_eq!(
            list_subscriptions(&dir).expect("list should succeed").len(),
            1
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
