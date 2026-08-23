//! # api::manga
//!
//! Manga source adapters: fetch a series' chapter list and resolve the page
//! images of a single chapter for CBZ packaging.
//!
//! ## Relationship to `api::novel`
//! The HTTP layer is shared: [`crate::api::novel::PoliteClient`] enforces the
//! per-host rate limits, retries, and browser-session reuse for both engines.
//! Only the payload differs — a novel chapter is text, a manga chapter is an
//! ordered list of image URLs.
//!
//! ## Responsibilities:
//! - `MangaSource` trait — the adapter contract
//! - Shared parsing helpers (chapter numbering, volume labels)
//! - `detect_source` — host-based adapter dispatch
//!
//! ## Adapter convention
//! All adapters return chapters **oldest-first** (reading order) and pages in
//! page order.  Adapters never download images themselves; they return URLs so
//! the caller controls pacing and caching.
//!
//! ## No generic fallback
//! Unlike novels, an unknown manga host gets **no** heuristic adapter: telling
//! a page image apart from a banner ad without site knowledge is guesswork,
//! and a silently wrong chapter is worse than a clear error.  New sites need
//! an explicit adapter.
//!
//! ## Dependencies:
//! - `scraper` – HTML parsing
//! - `api::novel` – shared HTTP client and URL helpers

pub mod fanfox;
pub mod madara;
pub mod mangatown;
pub mod packed;
pub mod themesia;
pub mod webtoons;

use crate::api::novel::PoliteClient;
use crate::error::{FeroError, Result};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Series metadata plus the full chapter list, as scraped from the source.
#[derive(Debug, Clone, Default)]
pub struct MangaInfo {
    /// Series title.
    pub title: String,
    /// Writer, if exposed by the source.
    pub author: Option<String>,
    /// Illustrator, where the source separates it from the writer.
    pub artist: Option<String>,
    /// Cover image URL, if exposed by the source.
    pub cover_url: Option<String>,
    /// Synopsis, if exposed by the source.
    pub description: Option<String>,
    /// `Some(true)` when the source marks the series as finished.
    pub completed_hint: Option<bool>,
    /// Genre names as listed by the source (may be empty).
    pub genres: Vec<String>,
    /// Free-form tags as listed by the source (may be empty).
    pub tags: Vec<String>,
    /// Right-to-left reading order (Japanese manga) vs. left-to-right
    /// (webtoons, western comics).  Stored into `ComicInfo.xml`.
    pub right_to_left: bool,
    /// All chapters in reading order (oldest first).
    pub chapters: Vec<MangaChapterRef>,
}

/// A single chapter reference from a series page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MangaChapterRef {
    /// Chapter title as listed on the series page.
    pub title: String,
    /// Absolute chapter URL — the identity key for deduplication.
    pub url: String,
    /// Volume label when the source groups chapters into volumes.
    pub volume: Option<String>,
    /// Chapter number as text (`10`, `10.5`) for `ComicInfo.xml`.
    pub number: Option<String>,
}

/// One page image of a chapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageImage {
    /// Absolute image URL.
    pub url: String,
    /// Page the image is embedded in.  Some CDNs (Webtoons) answer `403`
    /// without a matching `Referer`, so adapters record it here rather than
    /// letting the caller guess.
    pub referer: Option<String>,
}

impl PageImage {
    /// A page image that needs no referer.
    pub fn plain(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            referer: None,
        }
    }

    /// A page image that must be requested with `referer`.
    pub fn with_referer(url: impl Into<String>, referer: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            referer: Some(referer.into()),
        }
    }
}

/// Contract for one manga hosting site.
pub trait MangaSource {
    /// Stable adapter id (`mangatown`, `fanfox`, `webtoons`).
    fn id(&self) -> &'static str;

    /// Fetches the series page and parses metadata plus the full chapter list.
    fn fetch_series_info(&self, client: &PoliteClient, url: &str) -> Result<MangaInfo>;

    /// Resolves the ordered page images of one chapter.
    ///
    /// Implementations may need several requests (one per page on paginated
    /// readers); the shared client paces them.
    fn fetch_chapter_pages(
        &self,
        client: &PoliteClient,
        chapter: &MangaChapterRef,
    ) -> Result<Vec<PageImage>>;
}

/// Hosts with a manga adapter, for the user-facing error message.
const SUPPORTED_HOSTS: [&str; 6] = [
    "mangatown.com",
    "fanfox.net",
    "webtoons.com",
    "mangaread.org",
    "manhuaplus.com",
    "hentai20.io",
];

/// Sites running the Madara WordPress theme — one adapter serves them all.
const MADARA_HOSTS: [&str; 2] = ["mangaread.org", "manhuaplus.com"];

/// Sites running the Themesia WordPress theme.
const THEMESIA_HOSTS: [&str; 1] = ["hentai20.io"];

/// Picks the adapter for a subscription URL based on its host.
///
/// Returns `None` for unsupported hosts — see the module docs on why there is
/// no heuristic fallback.
pub fn detect_source(url: &str) -> Option<Box<dyn MangaSource>> {
    let host = crate::api::novel::host_of(url).unwrap_or_default();
    if host.ends_with("mangatown.com") {
        Some(Box::new(mangatown::MangaTownSource))
    } else if host.ends_with("fanfox.net") || host.ends_with("mangafox.la") {
        // FanFox and its mangafox.la mirror serve identical markup.
        Some(Box::new(fanfox::FanFoxSource))
    } else if host.ends_with("webtoons.com") {
        Some(Box::new(webtoons::WebtoonsSource))
    } else if matches_host(&host, &MADARA_HOSTS) {
        Some(Box::new(madara::MadaraSource))
    } else if matches_host(&host, &THEMESIA_HOSTS) {
        Some(Box::new(themesia::ThemesiaSource))
    } else {
        None
    }
}

/// Whether `host` is one of `candidates` or a subdomain of one.
fn matches_host(host: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| host == *candidate || host.ends_with(&format!(".{candidate}")))
}

/// Returns the adapter for `url`, or a German error naming the supported sites.
///
/// # Errors
/// - `FeroError::ExternalApi` when no adapter covers the URL's host
pub fn require_source(url: &str) -> Result<Box<dyn MangaSource>> {
    detect_source(url).ok_or_else(|| {
        FeroError::ExternalApi(format!(
            "Für diese Seite gibt es keinen Manga-Adapter. \
             Unterstützt werden derzeit: {}.",
            SUPPORTED_HOSTS.join(", ")
        ))
    })
}

// ---------------------------------------------------------------------------
// Shared parsing helpers
// ---------------------------------------------------------------------------

/// Extracts a chapter number from a chapter title or URL.
///
/// Sources write chapter numbers in many shapes (`Ch.12`, `Chapter 12.5`,
/// `c012`, `/c012.5/`); `ComicInfo.xml` wants the bare number, and readers
/// sort by it.  Decimals are preserved because half-chapters (`10.5`) are a
/// real and common thing in scanlations.
///
/// A number following a chapter marker wins.  Without a marker the **last**
/// number in the text is used, because sources that omit the marker put the
/// number at the end (`Barakamon 12`).  That misreads a title like `Area 51`,
/// which is acceptable: the number is cosmetic metadata, while reading order
/// comes from the chapter index.
pub fn extract_chapter_number(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for marker in ["chapter", "chap.", "chap", "ch.", "ch ", "c"] {
        let mut rest = lower.as_str();
        while let Some(position) = rest.find(marker) {
            let after = &rest[position + marker.len()..];
            if let Some(number) = leading_number(after.trim_start_matches([' ', '.', ':', '-'])) {
                return Some(number);
            }
            rest = &rest[position + marker.len()..];
        }
    }
    last_number(&lower)
}

/// Returns the last standalone number in `text`, if any.
fn last_number(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 {
        if !bytes[end - 1].is_ascii_digit() {
            end -= 1;
            continue;
        }
        let mut start = end;
        while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
            start -= 1;
        }
        return leading_number(&text[start..end]);
    }
    None
}

/// Extracts a volume label (`v01`) from a chapter URL or title, if present.
pub fn extract_volume(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for marker in ["/v", "vol.", "volume", "vol "] {
        if let Some(position) = lower.find(marker) {
            let after = &lower[position + marker.len()..];
            if let Some(number) = leading_number(after.trim_start_matches([' ', '.', ':', '-'])) {
                // TBD volumes appear as `v_TBD` upstream and carry no meaning.
                if number != "0" {
                    return Some(format!("v{number}"));
                }
            }
        }
    }
    None
}

/// Reads a leading decimal number, returning it without leading zeros.
fn leading_number(text: &str) -> Option<String> {
    let digits: String = text
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let trimmed = digits.trim_end_matches('.');
    if trimmed.is_empty() || !trimmed.starts_with(|ch: char| ch.is_ascii_digit()) {
        return None;
    }
    // `c007` and `c7` are the same chapter; normalize so both sort alike.
    let (whole, fraction) = match trimmed.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (trimmed, None),
    };
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    Some(match fraction {
        Some(fraction) if !fraction.is_empty() => format!("{whole}.{fraction}"),
        _ => whole.to_string(),
    })
}

/// Collapses runs of whitespace and trims — scraped text is full of newlines.
pub fn tidy_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sorts chapters ascending by their parsed chapter number.
///
/// Some series pages repeat the newest chapters in a "latest" block, so the
/// document order is not reliably newest-first and reversing it yields a
/// scrambled reading order.  The chapter number is the authoritative signal —
/// but only when *every* chapter carries one, otherwise the entries without a
/// number would all collapse to the front.  With a mixed or numberless list
/// the caller's existing order is kept untouched.
///
/// Returns `true` when the list was sorted.
pub fn sort_chapters_by_number(chapters: &mut [MangaChapterRef]) -> bool {
    let numbers: Option<Vec<f64>> = chapters
        .iter()
        .map(|chapter| {
            chapter
                .number
                .as_deref()
                .and_then(|n| n.parse::<f64>().ok())
        })
        .collect();
    let Some(numbers) = numbers else {
        return false;
    };

    let mut indexed: Vec<(f64, MangaChapterRef)> = numbers
        .into_iter()
        .zip(chapters.iter().cloned())
        .collect::<Vec<_>>();
    // Stable sort: chapters sharing a number keep their document order.
    indexed.sort_by(|(left, _), (right, _)| {
        left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
    });
    for (slot, (_, chapter)) in chapters.iter_mut().zip(indexed) {
        *slot = chapter;
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_hosts_only() {
        assert_eq!(
            detect_source("https://www.mangatown.com/manga/barakamon/").map(|source| source.id()),
            Some("mangatown")
        );
        assert_eq!(
            detect_source("https://fanfox.net/manga/chainsaw_man/").map(|source| source.id()),
            Some("fanfox")
        );
        assert_eq!(
            detect_source("https://www.webtoons.com/en/action/gul/list?title_no=2617")
                .map(|source| source.id()),
            Some("webtoons")
        );
        assert!(detect_source("https://example.com/manga/x").is_none());
    }

    #[test]
    fn theme_adapters_cover_every_site_on_that_theme() {
        for url in [
            "https://www.mangaread.org/manga/x/",
            "https://manhuaplus.com/manga/martial-peak/",
        ] {
            assert_eq!(
                detect_source(url).map(|source| source.id()),
                Some("madara"),
                "{url}"
            );
        }
        assert_eq!(
            detect_source("https://hentai20.io/manga/x/").map(|source| source.id()),
            Some("themesia")
        );
    }

    #[test]
    fn host_matching_accepts_subdomains_but_not_lookalikes() {
        assert!(matches_host("www.mangaread.org", &["mangaread.org"]));
        assert!(matches_host("mangaread.org", &["mangaread.org"]));
        // A domain merely ending in the same letters must not match.
        assert!(!matches_host("evilmangaread.org", &["mangaread.org"]));
    }

    #[test]
    fn unsupported_host_names_the_alternatives() {
        let error = require_source("https://example.com/manga/x")
            // `Box<dyn MangaSource>` is not `Debug`; the id stands in for it.
            .map(|source| source.id())
            .expect_err("unsupported host should error");
        let message = error.to_string();
        assert!(message.contains("mangatown.com"));
        assert!(message.contains("webtoons.com"));
    }

    #[test]
    fn extracts_chapter_numbers_from_common_shapes() {
        assert_eq!(extract_chapter_number("Chapter 12").as_deref(), Some("12"));
        assert_eq!(extract_chapter_number("Ch.7").as_deref(), Some("7"));
        assert_eq!(
            extract_chapter_number("Barakamon 10.5: Extra").as_deref(),
            Some("10.5")
        );
        assert_eq!(
            extract_chapter_number("https://www.mangatown.com/manga/x/v01/c012/").as_deref(),
            Some("12")
        );
        // Leading zeros must not survive, or `c007` and `c7` would differ.
        assert_eq!(extract_chapter_number("c007").as_deref(), Some("7"));
        // No marker: sources that omit it put the number last.
        assert_eq!(
            extract_chapter_number("Barakamon 12").as_deref(),
            Some("12")
        );
        assert_eq!(extract_chapter_number("Prologue").as_deref(), None);
    }

    #[test]
    fn extracts_volume_labels() {
        assert_eq!(
            extract_volume("https://www.mangatown.com/manga/x/v03/c012/").as_deref(),
            Some("v3")
        );
        assert_eq!(extract_volume("Vol. 12 Ch. 3").as_deref(), Some("v12"));
        assert_eq!(extract_volume("Chapter 3").as_deref(), None);
    }

    #[test]
    fn tidy_text_collapses_scraped_whitespace() {
        assert_eq!(tidy_text("  Kapitel \n\t 1  "), "Kapitel 1");
    }

    fn chapter(number: Option<&str>) -> MangaChapterRef {
        MangaChapterRef {
            title: format!("Chapter {}", number.unwrap_or("?")),
            url: format!("https://x.example/{}", number.unwrap_or("none")),
            volume: None,
            number: number.map(str::to_string),
        }
    }

    #[test]
    fn sorts_chapters_by_number_including_decimals() {
        // The order a "latest chapters" block leaves behind.
        let mut chapters = vec![
            chapter(Some("3860")),
            chapter(Some("1")),
            chapter(Some("10.5")),
            chapter(Some("2")),
            chapter(Some("3862")),
        ];
        assert!(sort_chapters_by_number(&mut chapters));

        let order: Vec<&str> = chapters
            .iter()
            .map(|c| c.number.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(order, ["1", "2", "10.5", "3860", "3862"]);
    }

    #[test]
    fn leaves_a_partially_numbered_list_untouched() {
        // Without a number on every entry, sorting would bunch the unnumbered
        // ones together and destroy the source's own order.
        let mut chapters = vec![chapter(Some("2")), chapter(None), chapter(Some("1"))];
        let before = chapters.clone();
        assert!(!sort_chapters_by_number(&mut chapters));
        assert_eq!(chapters, before);
    }
}
