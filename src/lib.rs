#![doc = "Fero core foundation."]

mod desktop;

pub mod api;
pub mod core;
pub mod deliver;
pub mod error;

pub use api::anilist::AniListClient;
pub use api::novel::{detect_source, ChapterRef, NovelInfo, NovelSource, PoliteClient};
pub use core::batching::{plan as plan_batches, FileKind, PlannedFile, DEFAULT_BATCH_SIZE};
pub use core::epub::{write_epub, EpubChapter, EpubMeta};
pub use core::status::{resolve as resolve_series_status, SeriesStatus};
pub use core::webnovel::{
    delete_subscription, list_subscriptions, load_subscription, save_subscription, BlocklistEntry,
    KnownChapter, Subscription,
};
pub use deliver::manifest::{ChapterRecord, DeliveredFile, WorkManifest};
pub use deliver::targets::{
    resolve_data_dir, resolve_target, DataDir, MediaKind, TargetResolution, TargetSettings,
    TargetSource,
};
pub use error::{FeroError, Result};

/// Starts the Fero desktop shell.
///
/// Fero subscribes to sources, downloads new items, packages them and hands
/// them to the library. Browsing, viewing and reading progress live in Fundus.
pub fn run() -> Result<()> {
    desktop::run()
}
