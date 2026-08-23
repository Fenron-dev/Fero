//! One-time move of Fero's bookkeeping out of the library.
//!
//! Until now subscriptions, the trash and the blocklist lived in
//! `<library>/.fero/`. That was wrong twice over: it put Fero's files inside
//! someone else's collection, and it made the subscription list unreachable
//! whenever the library — typically a network share — was offline.

use std::path::Path;

/// What the migration did, for logging and for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The data directory already holds a store, or the library has none.
    NothingToDo,
    /// Entries were copied across.
    Migrated {
        /// Number of subscription files copied.
        subscriptions: usize,
        /// Where they came from, so the log can name it.
        from: String,
    },
    /// Copying failed; the old location is untouched and still authoritative.
    Failed(String),
}

/// Directories and files that belong to Fero rather than to the library.
const MIGRATED_ENTRIES: &[&str] = &["webnovels", "mangas", "webnovel_blocklist.json"];

/// Copies Fero's bookkeeping from the library into its own data directory.
///
/// Deliberately a **copy**, not a move: if anything goes wrong the originals
/// are still there. The leftovers in the library are inert — nothing reads them
/// afterwards — and can be deleted by hand once the move is confirmed good.
///
/// Idempotent: as soon as the data directory holds any of the entries, this
/// does nothing, so it is safe to call on every start.
pub fn migrate_store_from_library(data_dir: &Path, library_system_dir: &Path) -> Outcome {
    let already_migrated = MIGRATED_ENTRIES
        .iter()
        .any(|entry| data_dir.join(entry).exists());
    if already_migrated {
        return Outcome::NothingToDo;
    }

    let present: Vec<&str> = MIGRATED_ENTRIES
        .iter()
        .copied()
        .filter(|entry| library_system_dir.join(entry).exists())
        .collect();
    if present.is_empty() {
        return Outcome::NothingToDo;
    }

    if let Err(error) = std::fs::create_dir_all(data_dir) {
        return Outcome::Failed(format!("Datenordner nicht anlegbar: {error}"));
    }

    let mut subscriptions = 0usize;
    for entry in present {
        let source = library_system_dir.join(entry);
        let target = data_dir.join(entry);
        let copied = if source.is_dir() {
            copy_dir(&source, &target)
        } else {
            std::fs::copy(&source, &target)
                .map(|_| 0)
                .map_err(|error| error.to_string())
        };
        match copied {
            Ok(count) => subscriptions += count,
            Err(error) => return Outcome::Failed(format!("{entry}: {error}")),
        }
    }

    Outcome::Migrated {
        subscriptions,
        from: library_system_dir.display().to_string(),
    }
}

/// Copies a directory tree, returning the number of `.json` files copied.
fn copy_dir(source: &Path, target: &Path) -> std::result::Result<usize, String> {
    std::fs::create_dir_all(target).map_err(|error| error.to_string())?;
    let mut count = 0usize;
    let entries = std::fs::read_dir(source).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if from.is_dir() {
            count += copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|error| error.to_string())?;
            if from.extension().and_then(|ext| ext.to_str()) == Some("json") {
                count += 1;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("fero-migrate-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    fn library_with_subscriptions(count: usize) -> (PathBuf, PathBuf) {
        let root = scratch("lib");
        let system = root.join(".fero");
        std::fs::create_dir_all(system.join("webnovels/trash")).expect("dirs should be creatable");
        for i in 0..count {
            std::fs::write(system.join(format!("webnovels/{i:016x}.json")), "{}")
                .expect("write should succeed");
        }
        std::fs::write(system.join("webnovel_blocklist.json"), "[]").expect("write should succeed");
        (root, system)
    }

    #[test]
    fn copies_subscriptions_and_blocklist() {
        let (_root, system) = library_with_subscriptions(3);
        let data = scratch("data");

        let outcome = migrate_store_from_library(&data, &system);

        match outcome {
            Outcome::Migrated { subscriptions, .. } => assert_eq!(subscriptions, 3),
            other => panic!("expected Migrated, got {other:?}"),
        }
        assert!(data.join("webnovel_blocklist.json").exists());
        assert!(data.join("webnovels/trash").is_dir(), "trash travels along");
    }

    /// The originals stay put — if anything went wrong they are still the only
    /// copy of the user's subscriptions.
    #[test]
    fn leaves_the_originals_alone() {
        let (_root, system) = library_with_subscriptions(2);
        let data = scratch("data");

        migrate_store_from_library(&data, &system);

        assert!(system.join("webnovels").is_dir());
        assert!(system.join("webnovel_blocklist.json").exists());
    }

    #[test]
    fn running_twice_changes_nothing() {
        let (_root, system) = library_with_subscriptions(2);
        let data = scratch("data");

        migrate_store_from_library(&data, &system);
        let second = migrate_store_from_library(&data, &system);

        assert_eq!(second, Outcome::NothingToDo);
    }

    #[test]
    fn without_a_library_store_there_is_nothing_to_do() {
        let empty = scratch("empty");
        let data = scratch("data");

        assert_eq!(
            migrate_store_from_library(&data, &empty),
            Outcome::NothingToDo
        );
    }
}
