//! Resolves two kinds of location: where Fero keeps its own data, and where a
//! finished work is delivered.
//!
//! Both follow the same rule: **never pick a location silently.** If nothing is
//! configured, or a configured location cannot be reached, the caller gets a
//! suggestion plus a reason and has to let the user decide. Downloading into a
//! surprise directory is worse than not downloading at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

/// Directory name used for the portable data folder next to the application.
const PORTABLE_DIR_NAME: &str = "Fero-Daten";

/// File that records a data directory the user picked because the portable one
/// was not writable. It lives in the home directory and holds nothing but a
/// path — the actual data is wherever it points.
const POINTER_FILE: &str = "data-location.json";

/// Sub-directory of the data folder used as the suggested download fallback.
const FALLBACK_DIR_NAME: &str = "Downloads";

// ---------------------------------------------------------------------------
// Media kinds
// ---------------------------------------------------------------------------

/// The kinds of media Fero can acquire.
///
/// Adding a kind means adding a variant here plus a source adapter — the
/// settings, the target chain and the UI derive themselves from [`MediaKind::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// Serialized web novels, delivered as EPUB.
    Webnovel,
    /// Manga and webtoons, delivered as CBZ.
    Manga,
    /// Podcast episodes, delivered as tagged audio files.
    Podcast,
}

impl MediaKind {
    /// Every kind, in the order the settings UI should show them.
    pub const ALL: &'static [MediaKind] =
        &[MediaKind::Webnovel, MediaKind::Manga, MediaKind::Podcast];

    /// Stable identifier used as the settings key and in the HTTP API.
    ///
    /// Must not change once released — it keys persisted settings.
    pub fn id(self) -> &'static str {
        match self {
            Self::Webnovel => "webnovel",
            Self::Manga => "manga",
            Self::Podcast => "podcast",
        }
    }

    /// Folder name used underneath a shared target, matching the Fundus media
    /// roots so a scan picks the works up without extra configuration.
    pub fn folder_segment(self) -> &'static str {
        match self {
            Self::Webnovel => "Webnovels",
            Self::Manga => "Manga",
            Self::Podcast => "Podcasts",
        }
    }

    /// Human-readable label for the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Webnovel => "Webnovels",
            Self::Manga => "Manga & Webtoons",
            Self::Podcast => "Podcasts",
        }
    }

    /// Parses an identifier produced by [`MediaKind::id`].
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.id() == id)
    }
}

// ---------------------------------------------------------------------------
// Data directory
// ---------------------------------------------------------------------------

/// Where Fero keeps subscriptions, settings, sessions and the chapter cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDir {
    /// The portable folder next to the application is usable.
    Portable(PathBuf),
    /// The user picked this folder because the portable one was not writable.
    Chosen(PathBuf),
    /// Nothing usable yet — the UI has to ask before Fero can store anything.
    NeedsSetup {
        /// Where Fero would like to put its data.
        suggestion: PathBuf,
        /// Why the portable location cannot be used, in German, for the UI.
        reason: String,
    },
}

impl DataDir {
    /// The usable path, or `None` while setup is still pending.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Portable(path) | Self::Chosen(path) => Some(path),
            Self::NeedsSetup { .. } => None,
        }
    }
}

/// Returns the directory the application bundle lives in.
///
/// On macOS the executable sits in `Fero.app/Contents/MacOS/`, so the bundle's
/// own parent is three levels up; elsewhere it is simply the executable's
/// directory.
fn application_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if dir.ends_with("Contents/MacOS") {
        dir.parent()?.parent()?.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

/// Path of the pointer file in the user's home directory.
fn pointer_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".fero").join(POINTER_FILE))
}

#[derive(Serialize, Deserialize)]
struct Pointer {
    data_dir: String,
}

/// Checks whether `dir` can hold Fero's data, creating it if necessary.
///
/// Creation alone is not proof: a directory can exist and still reject writes
/// (read-only volume, restricted bundle). So a probe file is written and
/// removed again.
fn is_usable(dir: &Path) -> std::result::Result<(), String> {
    if let Err(error) = std::fs::create_dir_all(dir) {
        return Err(format!("Ordner lässt sich nicht anlegen: {error}"));
    }
    let probe = dir.join(".fero-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(format!("Ordner ist nicht beschreibbar: {error}")),
    }
}

/// Resolves the data directory.
///
/// Order: the portable folder next to the application, then a folder the user
/// picked earlier. If neither works, the caller must ask — Fero does not fall
/// back to the home directory on its own, because then nobody would notice that
/// the portable setup is broken.
pub fn resolve_data_dir() -> DataDir {
    let portable = application_dir().map(|dir| dir.join(PORTABLE_DIR_NAME));

    if let Some(candidate) = portable.as_ref() {
        match is_usable(candidate) {
            Ok(()) => return DataDir::Portable(candidate.clone()),
            Err(reason) => {
                if let Some(chosen) = load_pointer() {
                    if is_usable(&chosen).is_ok() {
                        return DataDir::Chosen(chosen);
                    }
                }
                return DataDir::NeedsSetup {
                    suggestion: candidate.clone(),
                    reason: format!(
                        "Der Ordner neben der App ({}) ist nicht nutzbar: {reason}",
                        candidate.display()
                    ),
                };
            }
        }
    }

    if let Some(chosen) = load_pointer() {
        if is_usable(&chosen).is_ok() {
            return DataDir::Chosen(chosen);
        }
    }

    DataDir::NeedsSetup {
        suggestion: PathBuf::from(PORTABLE_DIR_NAME),
        reason: "Der Ort der Anwendung lässt sich nicht bestimmen.".to_string(),
    }
}

/// Reads the pointer file, if it exists and parses.
fn load_pointer() -> Option<PathBuf> {
    let path = pointer_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let pointer: Pointer = serde_json::from_str(&raw).ok()?;
    Some(PathBuf::from(pointer.data_dir))
}

/// Records a data directory the user picked.
///
/// # Errors
/// - [`VaultError::InvalidVaultPath`] if the directory cannot be written to
/// - [`VaultError::Io`] if the pointer file cannot be stored
pub fn set_data_dir(dir: &Path) -> Result<()> {
    is_usable(dir).map_err(VaultError::InvalidVaultPath)?;
    let path = pointer_path()
        .ok_or_else(|| VaultError::Io("Kein Home-Verzeichnis gefunden.".to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(VaultError::from)?;
    }
    let pointer = Pointer {
        data_dir: dir.to_string_lossy().to_string(),
    };
    let body = serde_json::to_string_pretty(&pointer)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    std::fs::write(&path, body).map_err(VaultError::from)
}

// ---------------------------------------------------------------------------
// Delivery targets
// ---------------------------------------------------------------------------

/// User-configured download targets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetSettings {
    /// Default target per media kind, keyed by [`MediaKind::id`].
    ///
    /// A configured path *is* the media folder — no kind segment is appended,
    /// so pointing Webnovels at `…/Fundus/Webnovels` does not produce
    /// `…/Fundus/Webnovels/Webnovels`.
    #[serde(default)]
    pub defaults: BTreeMap<String, String>,
    /// Shared fallback, used only after the user confirmed it once.
    ///
    /// Being shared across kinds, it *does* get a kind segment appended.
    #[serde(default)]
    pub fallback: Option<String>,
}

impl TargetSettings {
    /// Returns the configured default for `kind`, if any.
    pub fn default_for(&self, kind: MediaKind) -> Option<PathBuf> {
        self.defaults.get(kind.id()).map(PathBuf::from)
    }

    /// Sets or clears the default for `kind`.
    pub fn set_default(&mut self, kind: MediaKind, dir: Option<&Path>) {
        match dir {
            Some(dir) => {
                self.defaults
                    .insert(kind.id().to_string(), dir.to_string_lossy().to_string());
            }
            None => {
                self.defaults.remove(kind.id());
            }
        }
    }
}

/// Which level of the chain produced a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetSource {
    /// The subscription carries its own target.
    Subscription,
    /// The media kind's configured default.
    MediaKindDefault,
    /// The confirmed shared fallback.
    Fallback,
}

/// Outcome of resolving where a work should be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolution {
    /// Directory the work folder should be created in.
    Resolved {
        /// Parent directory for the work folder, kind segment already applied.
        parent: PathBuf,
        /// Which level of the chain answered.
        source: TargetSource,
    },
    /// Nothing usable — the UI has to ask before anything is downloaded.
    NeedsChoice {
        /// Sensible default to pre-fill the folder picker with.
        suggestion: PathBuf,
        /// Why no target could be used, in German, for the UI.
        reason: String,
    },
}

/// Returns true when a directory can plausibly be written to right now.
///
/// A configured target may sit on a network share that is currently offline.
/// The parent has to exist; the leaf may still be missing and gets created at
/// delivery time.
fn is_reachable(dir: &Path) -> bool {
    if dir.is_dir() {
        return true;
    }
    dir.parent().map(Path::is_dir).unwrap_or(false)
}

/// Resolves the delivery target for one work.
///
/// Chain: the subscription's own target, then the media kind's default, then
/// the confirmed fallback. Each level is skipped when it is not reachable, and
/// an unreachable *configured* target is reported rather than silently replaced
/// — otherwise a downloaded work would land somewhere the user never chose.
pub fn resolve_target(
    subscription_target: Option<&Path>,
    kind: MediaKind,
    settings: &TargetSettings,
    data_dir: &Path,
) -> TargetResolution {
    let suggestion = data_dir.join(FALLBACK_DIR_NAME).join(kind.folder_segment());

    if let Some(dir) = subscription_target {
        if is_reachable(dir) {
            return TargetResolution::Resolved {
                parent: dir.to_path_buf(),
                source: TargetSource::Subscription,
            };
        }
        return TargetResolution::NeedsChoice {
            suggestion,
            reason: format!(
                "Das für dieses Abo eingestellte Ziel ist nicht erreichbar: {}",
                dir.display()
            ),
        };
    }

    if let Some(dir) = settings.default_for(kind) {
        if is_reachable(&dir) {
            return TargetResolution::Resolved {
                parent: dir,
                source: TargetSource::MediaKindDefault,
            };
        }
        return TargetResolution::NeedsChoice {
            suggestion,
            reason: format!(
                "Das Standardziel für {} ist nicht erreichbar: {}",
                kind.label(),
                dir.display()
            ),
        };
    }

    if let Some(fallback) = settings.fallback.as_ref() {
        let parent = PathBuf::from(fallback).join(kind.folder_segment());
        if is_reachable(&parent) || is_reachable(Path::new(fallback)) {
            return TargetResolution::Resolved {
                parent,
                source: TargetSource::Fallback,
            };
        }
        return TargetResolution::NeedsChoice {
            suggestion,
            reason: format!("Der Ausweichordner ist nicht erreichbar: {fallback}"),
        };
    }

    TargetResolution::NeedsChoice {
        suggestion,
        reason: format!("Für {} ist noch kein Zielordner festgelegt.", kind.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(defaults: &[(MediaKind, &str)], fallback: Option<&str>) -> TargetSettings {
        let mut settings = TargetSettings::default();
        for (kind, dir) in defaults {
            settings.set_default(*kind, Some(Path::new(dir)));
        }
        settings.fallback = fallback.map(str::to_string);
        settings
    }

    #[test]
    fn media_kind_ids_round_trip() {
        for kind in MediaKind::ALL {
            assert_eq!(MediaKind::from_id(kind.id()), Some(*kind));
        }
    }

    #[test]
    fn subscription_target_wins_over_default() {
        let dir = tempdir();
        let own = dir.join("woanders");
        std::fs::create_dir_all(&own).expect("dir should be creatable");
        let settings = settings(&[(MediaKind::Webnovel, dir.to_str().unwrap())], None);

        let resolved = resolve_target(Some(&own), MediaKind::Webnovel, &settings, &dir);

        assert_eq!(
            resolved,
            TargetResolution::Resolved {
                parent: own,
                source: TargetSource::Subscription,
            }
        );
    }

    /// A per-kind default already *is* the media folder, so no second segment.
    #[test]
    fn media_kind_default_gets_no_extra_segment() {
        let dir = tempdir();
        let target = dir.join("Webnovels");
        std::fs::create_dir_all(&target).expect("dir should be creatable");
        let settings = settings(&[(MediaKind::Webnovel, target.to_str().unwrap())], None);

        let resolved = resolve_target(None, MediaKind::Webnovel, &settings, &dir);

        assert_eq!(
            resolved,
            TargetResolution::Resolved {
                parent: target,
                source: TargetSource::MediaKindDefault,
            }
        );
    }

    /// The fallback is shared by all kinds, so it *does* get a segment.
    #[test]
    fn fallback_gets_a_kind_segment() {
        let dir = tempdir();
        let settings = settings(&[], Some(dir.to_str().unwrap()));

        let resolved = resolve_target(None, MediaKind::Manga, &settings, &dir);

        assert_eq!(
            resolved,
            TargetResolution::Resolved {
                parent: dir.join("Manga"),
                source: TargetSource::Fallback,
            }
        );
    }

    #[test]
    fn without_any_configuration_the_user_is_asked() {
        let dir = tempdir();

        let resolved = resolve_target(None, MediaKind::Podcast, &TargetSettings::default(), &dir);

        match resolved {
            TargetResolution::NeedsChoice { suggestion, reason } => {
                assert_eq!(suggestion, dir.join("Downloads").join("Podcasts"));
                assert!(
                    reason.contains("Podcasts"),
                    "reason names the kind: {reason}"
                );
            }
            other => panic!("expected NeedsChoice, got {other:?}"),
        }
    }

    /// An unreachable configured target must not silently fall through to the
    /// next level — the work would land somewhere the user never chose.
    #[test]
    fn unreachable_target_is_reported_not_replaced() {
        let dir = tempdir();
        let offline = Path::new("/Volumes/nicht-verbunden/Webnovels");
        let settings = settings(&[], Some(dir.to_str().unwrap()));

        let resolved = resolve_target(Some(offline), MediaKind::Webnovel, &settings, &dir);

        match resolved {
            TargetResolution::NeedsChoice { reason, .. } => {
                assert!(reason.contains("nicht erreichbar"), "got: {reason}");
            }
            other => panic!("expected NeedsChoice, got {other:?}"),
        }
    }

    #[test]
    fn clearing_a_default_removes_it() {
        let mut settings = TargetSettings::default();
        settings.set_default(MediaKind::Manga, Some(Path::new("/tmp/x")));
        assert!(settings.default_for(MediaKind::Manga).is_some());

        settings.set_default(MediaKind::Manga, None);

        assert!(settings.default_for(MediaKind::Manga).is_none());
    }

    /// Creates a unique scratch directory without pulling in a test-only crate.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fero-targets-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }
}
