//! Media domain types and property containers.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use crate::core::vault::RelativePath;

/// All supported media types in the current foundation.
pub const ALL_MEDIA_TYPES: [MediaType; 26] = [
    MediaType::Film,
    MediaType::Series,
    MediaType::Anime,
    MediaType::HentaiAnime,
    MediaType::HentaiGame,
    MediaType::HentaiManga,
    MediaType::Book,
    MediaType::Ebook,
    MediaType::Webnovel,
    MediaType::Comic,
    MediaType::Manga,
    MediaType::MusicAlbum,
    MediaType::MusicTrack,
    MediaType::Podcast,
    MediaType::Audiobook,
    MediaType::BoardGame,
    MediaType::RPG,
    MediaType::VideoGame,
    MediaType::Document,
    MediaType::Photo,
    MediaType::VideoMisc,
    MediaType::Font,
    MediaType::Software,
    MediaType::Model3D,
    MediaType::Archive,
    MediaType::Image,
];

/// Supported media categories for the foundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    /// Movies and feature films.
    Film,
    /// TV series and episodic video content.
    Series,
    /// Non-adult anime.
    Anime,
    /// Adult anime.
    HentaiAnime,
    /// Adult visual novels and games.
    HentaiGame,
    /// Adult manga and doujinshi.
    HentaiManga,
    /// Printed and digital books.
    Book,
    /// E-books with book-like treatment.
    Ebook,
    /// Serialized web novels downloaded chapter-by-chapter from the web.
    Webnovel,
    /// Comics and graphic novels.
    Comic,
    /// Manga.
    Manga,
    /// Full music albums.
    MusicAlbum,
    /// Individual tracks or singles.
    MusicTrack,
    /// Podcasts.
    Podcast,
    /// Audiobooks.
    Audiobook,
    /// Board games.
    BoardGame,
    /// Tabletop RPGs.
    RPG,
    /// Video games.
    VideoGame,
    /// Documents and office files.
    Document,
    /// Photographs and still images.
    Photo,
    /// Miscellaneous videos.
    VideoMisc,
    /// Font files.
    Font,
    /// Software packages and installers.
    Software,
    /// 3D models.
    Model3D,
    /// Archives.
    Archive,
    /// Generic images.
    Image,
    /// Unknown or not yet classified.
    Unclassified,
}

impl MediaType {
    /// Returns every supported media type except the unclassified bucket.
    pub fn all() -> &'static [MediaType] {
        &ALL_MEDIA_TYPES
    }

    /// Returns the default vault folder segment for this media type.
    pub fn folder_segment(self) -> &'static str {
        match self {
            Self::Film => "Filme",
            Self::Series => "Serien",
            Self::Anime | Self::HentaiAnime => "Anime",
            Self::HentaiGame | Self::HentaiManga => "Hentai",
            // Books and Ebooks share one folder — they differ only in format, not in content.
            Self::Book | Self::Ebook => "Bücher",
            // Webnovels get their own root: they are living serials managed by
            // the subscription engine, not static imported files.
            Self::Webnovel => "Webnovels",
            Self::Comic => "Comics",
            Self::Manga => "Manga",
            // Albums and tracks live under the same Musik root; artist/album subfolders are
            // added by the path builder once metadata is available.
            Self::MusicAlbum | Self::MusicTrack => "Musik",
            Self::Podcast => "Podcasts",
            Self::Audiobook => "Hörbücher",
            Self::BoardGame => "Brettspiele",
            Self::RPG => "TTRPG",
            Self::VideoGame => "Games",
            Self::Document => "Dokumente",
            Self::Photo => "Fotos",
            Self::VideoMisc => "Videos",
            Self::Font => "Schriften",
            Self::Software => "Software",
            Self::Model3D => "3D-Modelle",
            Self::Archive => "Archive",
            Self::Image => "Bilder",
            Self::Unclassified => "Unklassifiziert",
        }
    }

    /// Returns the preferred API provider for this media type.
    pub fn preferred_provider(self) -> Option<&'static str> {
        match self {
            Self::Film | Self::Series => Some("tmdb"),
            Self::Anime | Self::HentaiAnime => Some("anilist"),
            Self::Book | Self::Ebook | Self::Audiobook => Some("openlibrary"),
            // Metadata comes from the subscribed source site itself.
            Self::Webnovel => None,
            Self::Comic | Self::Manga | Self::HentaiManga => Some("mangadex"),
            Self::MusicAlbum | Self::MusicTrack => Some("musicbrainz"),
            Self::Podcast => Some("podcastindex"),
            Self::BoardGame => Some("bgg"),
            Self::RPG => Some("rpggeek"),
            Self::VideoGame => Some("igdb"),
            Self::HentaiGame => Some("vndb"),
            Self::Document
            | Self::Photo
            | Self::VideoMisc
            | Self::Font
            | Self::Software
            | Self::Model3D
            | Self::Archive
            | Self::Image
            | Self::Unclassified => None,
        }
    }

    /// Returns whether the type is explicitly adult-oriented.
    pub fn is_adult(self) -> bool {
        matches!(
            self,
            Self::HentaiAnime | Self::HentaiGame | Self::HentaiManga
        )
    }
}

impl Display for MediaType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Film => "film",
            Self::Series => "series",
            Self::Anime => "anime",
            Self::HentaiAnime => "hentai-anime",
            Self::HentaiGame => "hentai-game",
            Self::HentaiManga => "hentai-manga",
            Self::Book => "book",
            Self::Ebook => "ebook",
            Self::Webnovel => "webnovel",
            Self::Comic => "comic",
            Self::Manga => "manga",
            Self::MusicAlbum => "music-album",
            Self::MusicTrack => "music-track",
            Self::Podcast => "podcast",
            Self::Audiobook => "audiobook",
            Self::BoardGame => "board-game",
            Self::RPG => "rpg",
            Self::VideoGame => "video-game",
            Self::Document => "document",
            Self::Photo => "photo",
            Self::VideoMisc => "video-misc",
            Self::Font => "font",
            Self::Software => "software",
            Self::Model3D => "3d-model",
            Self::Archive => "archive",
            Self::Image => "image",
            Self::Unclassified => "unclassified",
        })
    }
}

/// Source of a metadata or property value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertySource {
    /// Fetched from an external API.
    Api,
    /// Produced by local AI analysis.
    Ai,
    /// Entered by the user.
    User,
    /// Produced by a workflow rule.
    Workflow,
    /// Generated by the system.
    System,
}

impl Display for PropertySource {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Api => "api",
            Self::Ai => "ai",
            Self::User => "user",
            Self::Workflow => "workflow",
            Self::System => "system",
        })
    }
}

/// Standard lifecycle states for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaStatus {
    /// The file has been ingested into the inbox.
    Inbox,
    /// The file needs manual review.
    NeedsReview,
    /// The item is in the managed library.
    InLibrary,
    /// The item is on a wishlist.
    Wishlist,
    /// The item has been completed.
    Completed,
    /// The item is currently on hold.
    OnHold,
    /// The item is archived.
    Archived,
    /// The item should be ignored by workflows.
    Ignored,
}

impl Display for MediaStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inbox => "inbox",
            Self::NeedsReview => "needs-review",
            Self::InLibrary => "in-library",
            Self::Wishlist => "wishlist",
            Self::Completed => "completed",
            Self::OnHold => "on-hold",
            Self::Archived => "archived",
            Self::Ignored => "ignored",
        })
    }
}

/// Portable metadata for a media entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaProperties {
    /// Current display title.
    pub title: Option<String>,
    /// German title when present.
    pub title_de: Option<String>,
    /// Original-language title.
    pub title_original: Option<String>,
    /// Author or creator name (books, webnovels, audiobooks).
    pub author: Option<String>,
    /// Main description text.
    pub description: Option<String>,
    /// German description text.
    pub description_de: Option<String>,
    /// Original-language description.
    pub description_original: Option<String>,
    /// Release year.
    pub year: Option<u16>,
    /// Primary language code.
    pub language: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Provider genres.
    pub genres: Vec<String>,
    /// User-defined or provider categories.
    pub categories: Vec<String>,
    /// Current status.
    pub status: Option<MediaStatus>,
    /// User rating.
    pub rating: Option<f32>,
    /// Provider rating.
    pub rating_external: Option<f32>,
    /// AniList identifier when available.
    pub anilist_id: Option<u32>,
    /// AniList detail URL.
    pub anilist_url: Option<String>,
    /// Canonical series title for episodic content.
    pub series_title: Option<String>,
    /// Number of the current season when known.
    pub season_number: Option<u16>,
    /// Season label or arc title when known.
    pub season_name: Option<String>,
    /// Start episode number for the file.
    pub episode_start: Option<u16>,
    /// End episode number for the file.
    pub episode_end: Option<u16>,
    /// Optional episode title.
    pub episode_title: Option<String>,
    /// Total episode count in the source series.
    pub episode_count: Option<u16>,
    /// Episode or runtime length in minutes.
    pub runtime_minutes: Option<u16>,
    /// Average community score from the provider.
    pub average_score: Option<f32>,
    /// Provider format, for example TV or Movie.
    pub format: Option<String>,
    /// Seasonal airing label.
    pub airing_season: Option<String>,
    /// Relative path to the assigned cover image.
    pub cover_path: Option<RelativePath>,
    /// Relative path to the generated thumbnail image.
    pub thumbnail_path: Option<RelativePath>,
    /// Optional note field for free text.
    pub notes: Option<String>,
    /// Additional user-defined properties.
    pub custom_fields: BTreeMap<String, String>,
    /// Import batch identifier if the entry came from an import run.
    pub import_batch_id: Option<String>,
}

impl Default for MediaProperties {
    fn default() -> Self {
        Self {
            title: None,
            title_de: None,
            title_original: None,
            author: None,
            description: None,
            description_de: None,
            description_original: None,
            year: None,
            language: None,
            tags: Vec::new(),
            genres: Vec::new(),
            categories: Vec::new(),
            status: Some(MediaStatus::Inbox),
            rating: None,
            rating_external: None,
            anilist_id: None,
            anilist_url: None,
            series_title: None,
            season_number: None,
            season_name: None,
            episode_start: None,
            episode_end: None,
            episode_title: None,
            episode_count: None,
            runtime_minutes: None,
            average_score: None,
            format: None,
            airing_season: None,
            cover_path: None,
            thumbnail_path: None,
            notes: None,
            custom_fields: BTreeMap::new(),
            import_batch_id: None,
        }
    }
}

impl MediaProperties {
    /// Returns the preferred title for display purposes.
    pub fn display_title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .or(self.title_de.as_deref())
            .or(self.title_original.as_deref())
    }
}

/// A concrete media entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEntry {
    /// Stable entry identifier.
    pub id: String,
    /// Media category.
    pub media_type: MediaType,
    /// Vault-relative path to the primary file.
    pub relative_path: RelativePath,
    /// Original file name at import time.
    pub original_filename: String,
    /// Portable metadata and user-edited properties.
    pub properties: MediaProperties,
    /// Source that created the current metadata snapshot.
    pub source: PropertySource,
    /// Creation time as UNIX seconds.
    pub created_at_unix: u64,
    /// Last update time as UNIX seconds.
    pub updated_at_unix: u64,
}

impl MediaEntry {
    /// Creates a new media entry skeleton.
    pub fn new(
        id: impl Into<String>,
        media_type: MediaType,
        relative_path: RelativePath,
        original_filename: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            media_type,
            relative_path,
            original_filename: original_filename.into(),
            properties: MediaProperties::default(),
            source: PropertySource::System,
            created_at_unix: 0,
            updated_at_unix: 0,
        }
    }

    /// Returns the best display title available for this entry.
    pub fn display_title(&self) -> &str {
        self.properties
            .display_title()
            .unwrap_or(self.original_filename.as_str())
    }
}
