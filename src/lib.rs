#![doc = "Fero core foundation."]

mod desktop;
mod desktop_manga;

pub mod api;
pub mod core;
pub mod error;
pub mod media;

pub use api::anilist::AniListClient;
pub use api::novel::{detect_source, ChapterRef, NovelInfo, NovelSource, PoliteClient};
pub use core::covers::{CoverCandidate, CoverFallbackChain, CoverSource};
pub use core::duplicate::{compute_fingerprint, compute_fingerprint_for_file, FileFingerprint};
pub use core::epub::{write_epub, EpubChapter, EpubMeta};
pub use core::import::{
    ClassificationSource, DuplicatePolicy, FileClassification, ImportConfig, ImportPlan,
    ImportPlanItem, ImportPlanner, ImportSummary, IncomingFile, PlannedImportStep,
    ResolvedMetadata, UserPrompt,
};
pub use core::properties::{render_sidecar_yaml, sidecar_path_for};
pub use core::vault::{RelativePath, Vault};
pub use core::webnovel::{
    delete_subscription, list_subscriptions, load_subscription, save_subscription, BlocklistEntry,
    KnownChapter, Subscription,
};
pub use error::{Result, VaultError};
pub use media::{
    MediaEntry, MediaProperties, MediaStatus, MediaType, PropertySource, ALL_MEDIA_TYPES,
};

/// Starts the Fero desktop shell.
///
/// The core modules still expose the vault, import, and metadata primitives;
/// this entry point now opens the first testable Tauri window.
pub fn run() -> Result<()> {
    desktop::run()
}
