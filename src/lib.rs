#![doc = "Fero core foundation."]

mod desktop;
mod desktop_manga;

pub mod api;
pub mod core;
pub mod deliver;
pub mod error;
pub mod media;

pub use api::anilist::AniListClient;
pub use api::novel::{detect_source, ChapterRef, NovelInfo, NovelSource, PoliteClient};
pub use core::covers::{CoverCandidate, CoverFallbackChain, CoverSource};
pub use core::epub::{write_epub, EpubChapter, EpubMeta};
pub use core::vault::{RelativePath, Vault};
pub use core::webnovel::{
    delete_subscription, list_subscriptions, load_subscription, save_subscription, BlocklistEntry,
    KnownChapter, Subscription,
};
pub use deliver::manifest::{ChapterRecord, DeliveredFile, SeriesStatus, WorkManifest};
pub use deliver::targets::{
    resolve_data_dir, resolve_target, DataDir, MediaKind, TargetResolution, TargetSettings,
    TargetSource,
};
pub use error::{Result, VaultError};
pub use media::{
    MediaEntry, MediaProperties, MediaStatus, MediaType, PropertySource, ALL_MEDIA_TYPES,
};

/// Starts the Fero desktop shell.
///
/// Fero subscribes to sources, downloads new items, packages them and hands
/// them to the library. Browsing, viewing and reading progress live in Fundus.
pub fn run() -> Result<()> {
    desktop::run()
}
