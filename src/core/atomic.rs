//! # core::atomic
//!
//! Crash-safe replacement of small state files.
//!
//! Fero has no database: a subscription, the blocklist, the schedule settings
//! and a work manifest are each a single JSON file. Writing one in place means
//! truncating it first, so an abort in that moment — a crash, a pulled power
//! cord, a `SIGKILL` from the tray — leaves half a file behind. And a half file
//! is worse than a missing one: the list view skips a subscription it cannot
//! parse, so the entry silently disappears from the UI while its delivered
//! files stay on disk and the next run downloads everything again.
//!
//! The fix is the classic one: write the new content to a temporary file in the
//! same directory, flush it to the platform, then `rename` it onto the target.
//! A rename within one filesystem is atomic, so every reader sees either the
//! old file or the new one — never a mixture. The temporary file is created
//! beside the target rather than in the system temp directory, because a rename
//! across filesystems is not a rename but a copy, and would lose exactly the
//! guarantee this module exists for.
//!
//! What this does *not* promise: that the rename itself survives a crash. That
//! would need the containing directory to be flushed too, and the failure it
//! protects against — losing the newest write, but keeping an intact older
//! file — is one Fero recovers from by checking the source again.
//!
//! ## Dependencies:
//! - `error` – `FeroError::Io`

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{FeroError, Result};

/// Distinguishes two temporary files when two threads replace the same path.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `bytes`, atomically as far as the filesystem allows.
///
/// # Errors
/// - [`FeroError::Io`] if the temporary file cannot be written or renamed
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_inner(path, bytes, false)
}

/// Like [`write_atomic`], but the file only ever exists with `0600`.
///
/// Used for the session store: it holds login cookies, and the permissions are
/// set on the temporary file *before* the rename so there is no moment in which
/// the finished file is readable by other accounts.
///
/// # Errors
/// - [`FeroError::Io`] if the temporary file cannot be written or renamed
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_inner(path, bytes, true)
}

fn write_atomic_inner(path: &Path, bytes: &[u8], owner_only: bool) -> Result<()> {
    let temporary = temporary_path(path)?;

    // A leftover temporary file from a previous crash must not make this write
    // fail, so the handle is opened with truncation rather than exclusively.
    let outcome = write_and_persist(&temporary, bytes, owner_only).and_then(|()| {
        fs::rename(&temporary, path).map_err(|error| {
            FeroError::Io(format!(
                "{} konnte nicht ersetzt werden: {error}",
                path.display()
            ))
        })
    });

    if outcome.is_err() {
        // Without this, a full disk would leave a `.fero-tmp` file next to
        // every state file it failed to replace.
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

fn write_and_persist(temporary: &Path, bytes: &[u8], owner_only: bool) -> Result<()> {
    let mut file = File::create(temporary).map_err(|error| {
        FeroError::Io(format!(
            "{} konnte nicht angelegt werden: {error}",
            temporary.display()
        ))
    })?;
    if owner_only {
        restrict_to_owner(&file);
    }
    file.write_all(bytes).map_err(|error| {
        FeroError::Io(format!(
            "{} konnte nicht geschrieben werden: {error}",
            temporary.display()
        ))
    })?;
    // Without this the rename can be visible before the content is, which on a
    // crash yields an intact-looking but empty file — the one outcome this
    // module must not produce.
    file.sync_all().map_err(|error| {
        FeroError::Io(format!(
            "{} konnte nicht gesichert werden: {error}",
            temporary.display()
        ))
    })
}

#[cfg(unix)]
fn restrict_to_owner(file: &File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_to_owner(_file: &File) {
    // Windows inherits the parent directory's ACL, which is the user's profile
    // for every location Fero stores sessions in.
}

/// Temporary name beside the target, so the rename stays within one filesystem.
fn temporary_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        FeroError::Io(format!(
            "{} hat kein übergeordnetes Verzeichnis.",
            path.display()
        ))
    })?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let ticket = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{stem}.fero-tmp-{}-{ticket}",
        std::process::id()
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fero-atomic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    #[test]
    fn writes_new_content_and_leaves_no_temporary_behind() {
        let dir = temp_dir("fresh");
        let target = dir.join("state.json");

        write_atomic(&target, b"{\"a\":1}").expect("write should succeed");

        assert_eq!(
            fs::read_to_string(&target).expect("file should exist"),
            "{\"a\":1}"
        );
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("dir should be readable")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("fero-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file survived the write");
    }

    #[test]
    fn replaces_existing_content_completely() {
        let dir = temp_dir("replace");
        let target = dir.join("state.json");
        write_atomic(&target, b"a much longer previous content").expect("first write");

        write_atomic(&target, b"short").expect("second write");

        assert_eq!(
            fs::read_to_string(&target).expect("file should exist"),
            "short"
        );
    }

    #[test]
    fn a_leftover_temporary_file_does_not_block_the_next_write() {
        let dir = temp_dir("leftover");
        let target = dir.join("state.json");
        // Same shape a crashed write would leave behind.
        fs::write(temporary_path(&target).expect("name"), b"garbage").expect("leftover");

        write_atomic(&target, b"fresh").expect("write should succeed");

        assert_eq!(
            fs::read_to_string(&target).expect("file should exist"),
            "fresh"
        );
    }

    #[test]
    fn a_missing_directory_is_an_error_rather_than_a_panic() {
        let dir = temp_dir("missing");
        let target = dir.join("nested").join("state.json");

        assert!(write_atomic(&target, b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_private_variant_is_never_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("private");
        let target = dir.join("sessions.json");

        write_atomic_private(&target, b"[]").expect("write should succeed");

        let mode = fs::metadata(&target)
            .expect("file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
