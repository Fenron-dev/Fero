//! Moving a delivered work folder from one parent to another.
//!
//! Exists for one situation: the network target was offline, the files were
//! staged locally, and now the share is back — the user presses "move" and the
//! whole work folder travels to where it was always meant to go.

use std::path::Path;

use crate::error::{FeroError, Result};

/// Moves a directory, surviving a filesystem boundary.
///
/// `rename` is tried first — instant on the same filesystem. Across
/// filesystems (local disk → network share, the whole point of this module) it
/// fails, so the fallback copies everything and removes the source only after
/// the copy went through completely. A half-moved work is never left behind:
/// either the source still exists in full, or the target does.
///
/// If the target directory already exists, the contents are merged file by
/// file. That happens when some chapters were delivered to the share before it
/// went offline and the rest were staged — both halves belong together.
///
/// # Errors
/// [`FeroError::Io`] when copying fails; the source is left untouched then.
pub fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Err(FeroError::Io(format!(
            "Quellordner fehlt: {}",
            from.display()
        )));
    }

    if !to.exists() && std::fs::rename(from, to).is_ok() {
        return Ok(());
    }

    copy_dir(from, to)?;
    std::fs::remove_dir_all(from).map_err(FeroError::from)
}

/// Recursively copies a directory tree.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).map_err(FeroError::from)?;
    for entry in std::fs::read_dir(from).map_err(FeroError::from)? {
        let entry = entry.map_err(FeroError::from)?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(FeroError::from)?;
        }
    }
    Ok(())
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
            std::env::temp_dir().join(format!("fero-mover-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    fn work_folder(base: &Path) -> PathBuf {
        let work = base.join("Titel");
        std::fs::create_dir_all(work.join(".chapters")).expect("dirs should be creatable");
        std::fs::write(work.join("Titel - 0001-0050.epub"), b"epub").expect("write");
        std::fs::write(work.join("fero.info.json"), b"{}").expect("write");
        std::fs::write(work.join(".chapters/0001.json"), b"{}").expect("write");
        work
    }

    #[test]
    fn moves_a_tree_completely() {
        let staging = scratch("staging");
        let target = scratch("target");
        let work = work_folder(&staging);

        move_dir(&work, &target.join("Titel")).expect("move should succeed");

        assert!(!work.exists(), "source is gone");
        assert!(target.join("Titel/Titel - 0001-0050.epub").exists());
        assert!(target.join("Titel/.chapters/0001.json").exists());
    }

    /// Chapters delivered before the share went offline plus staged ones —
    /// both halves belong together, so an existing target is merged into.
    #[test]
    fn merges_into_an_existing_target() {
        let staging = scratch("staging");
        let target = scratch("target");
        let work = work_folder(&staging);
        let existing = target.join("Titel");
        std::fs::create_dir_all(&existing).expect("dirs");
        std::fs::write(existing.join("Titel - 0051-0100.epub"), b"alt").expect("write");

        move_dir(&work, &existing).expect("merge should succeed");

        assert!(existing.join("Titel - 0001-0050.epub").exists(), "moved");
        assert!(existing.join("Titel - 0051-0100.epub").exists(), "kept");
        assert!(!work.exists());
    }

    #[test]
    fn a_missing_source_is_an_error() {
        let target = scratch("target");
        assert!(move_dir(Path::new("/gibt/es/nicht"), &target.join("x")).is_err());
    }
}
