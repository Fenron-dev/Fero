//! Where a comic's life cycle comes from.
//!
//! The counterpart to [`crate::api::novel::status`], and deliberately not the
//! same shape. A novel's status is scraped off one NovelUpdates page; a comic's
//! comes from a database with an API — AniList automatically, MyAnimeList when
//! the user pins that entry instead.
//!
//! ## Why two databases
//! AniList is the automatic path: no key to register, and the client is already
//! wired up for metadata enrichment. MyAnimeList exists because the automatic
//! path cannot always work. Scanlation titles and database titles drift apart
//! ("Max Level Player" vs. "The Maxed-out Player"), and no amount of fuzzy
//! matching reliably bridges that — only the reader knows which entry is the
//! one they are following. Pinning a link is that answer, and it should not
//! force a choice of database on them. MAL is read through Jikan rather than
//! the official API, which needs a registered client id: this repository is
//! public and has nowhere to keep one.
//!
//! ## What this module does not decide
//! Nothing here says "finished". These are facts about someone else's
//! database; what they mean for a subscription is decided once, in
//! [`crate::core::status::resolve_comic`].

use serde::Deserialize;

use crate::api::anilist::AniListClient;
use crate::api::novel::status::OriginalStatus;
use crate::api::novel::PoliteClient;
use crate::error::{FeroError, Result};

/// Jikan's manga endpoint — the keyless read path to MyAnimeList.
const JIKAN_MANGA_ENDPOINT: &str = "https://api.jikan.moe/v4/manga/";

/// One database's answer about a series.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComicEntry {
    /// Publication status of the original work.
    pub publication: OriginalStatus,
    /// The entry's own title, for the log line when a match is in doubt.
    pub title: Option<String>,
    /// Link to the entry, so a match found by search can be pinned afterwards.
    pub url: Option<String>,
}

/// A database a pinned status-source URL can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSource {
    /// `anilist.co/manga/<id>`
    AniList(u32),
    /// `myanimelist.net/manga/<id>`
    MyAnimeList(u32),
}

/// Recognizes a pinned status source; `None` for anything else.
///
/// Only the id is kept. The slug in these URLs is decorative, changes when a
/// title is renamed, and is not worth storing a second copy of.
pub fn parse_source_url(url: &str) -> Option<StatusSource> {
    // Lowercased throughout: digits are unaffected by case, so the byte
    // offsets stay valid for slicing out the id.
    let lower = url.to_lowercase();
    if lower.contains("anilist.co") {
        return id_after(&lower, "/manga/")
            .or_else(|| id_after(&lower, "/media/"))
            .map(StatusSource::AniList);
    }
    if lower.contains("myanimelist.net") {
        return id_after(&lower, "/manga/").map(StatusSource::MyAnimeList);
    }
    None
}

/// Reads the entry a subscription's status source URL points at.
///
/// Returns `Ok(None)` for a URL that names no known database — a bad link is
/// the user's to fix, not an error worth failing a check run over.
///
/// # Errors
/// - `FeroError::ExternalApi` when the database is unreachable or has no such
///   entry
pub fn fetch_by_url(client: &PoliteClient, url: &str) -> Result<Option<ComicEntry>> {
    match parse_source_url(url) {
        Some(StatusSource::AniList(id)) => fetch_anilist(id).map(Some),
        Some(StatusSource::MyAnimeList(id)) => fetch_myanimelist(client, id).map(Some),
        None => Ok(None),
    }
}

/// Looks a series up by title on AniList.
///
/// `Ok(None)` covers both "nothing found" and "found something that does not
/// convincingly match" — see [`crate::api::anilist::titles_match`]. The two are
/// the same answer here: not sure enough to act on.
///
/// # Errors
/// - `FeroError::ExternalApi` on transport failures
pub fn search(title: &str) -> Result<Option<ComicEntry>> {
    let anilist = AniListClient::default();
    Ok(anilist.search_manga(title)?.map(|hit| ComicEntry {
        publication: classify(hit.status.as_deref().unwrap_or_default()),
        title: hit.title,
        url: hit.anilist_url,
    }))
}

/// Maps a database's publication status onto the shared vocabulary.
///
/// Handles both spellings: AniList shouts in enum case (`NOT_YET_RELEASED`),
/// MyAnimeList writes prose (`On Hiatus`). Order matters — "Not yet published"
/// contains "publish", and reading it as "still publishing" would mark a work
/// that has not started as running.
pub fn classify(status: &str) -> OriginalStatus {
    let lower = status.to_lowercase();
    if lower.contains("hiatus") {
        OriginalStatus::Hiatus
    } else if lower.contains("cancel") || lower.contains("discontinued") {
        OriginalStatus::Dropped
    } else if lower.contains("not yet") || lower.contains("not_yet") {
        OriginalStatus::Unknown
    } else if lower.contains("finish") || lower.contains("complete") {
        OriginalStatus::Completed
    } else if lower.contains("releasing")
        || lower.contains("publishing")
        || lower.contains("ongoing")
    {
        OriginalStatus::Ongoing
    } else {
        OriginalStatus::Unknown
    }
}

/// Fetches one AniList entry by id.
fn fetch_anilist(id: u32) -> Result<ComicEntry> {
    let anilist = AniListClient::default();
    let entry = anilist
        .manga_by_id(id)?
        .ok_or_else(|| FeroError::ExternalApi(format!("AniList kennt den Eintrag {id} nicht.")))?;
    Ok(ComicEntry {
        publication: classify(entry.status.as_deref().unwrap_or_default()),
        title: entry.title,
        url: entry.anilist_url,
    })
}

/// Fetches one MyAnimeList entry by id, through Jikan.
///
/// Goes through [`PoliteClient`] like every other outbound request, so Jikan's
/// rate limit is covered by the same per-host pacing as the scrapers.
fn fetch_myanimelist(client: &PoliteClient, id: u32) -> Result<ComicEntry> {
    let (_, body) = client.get_text(&format!("{JIKAN_MANGA_ENDPOINT}{id}"))?;
    parse_jikan(&body, id)
}

/// Maps a Jikan response body onto an entry. Split out to stay testable.
fn parse_jikan(body: &str, id: u32) -> Result<ComicEntry> {
    let payload: JikanResponse = serde_json::from_str(body)
        .map_err(|error| FeroError::ExternalApi(format!("Antwort von Jikan unlesbar: {error}")))?;
    let data = payload.data.ok_or_else(|| {
        FeroError::ExternalApi(format!("MyAnimeList kennt den Eintrag {id} nicht."))
    })?;
    Ok(ComicEntry {
        publication: classify(data.status.as_deref().unwrap_or_default()),
        title: data.title,
        url: data.url,
    })
}

/// The id following a path marker, e.g. `12345` in `/manga/12345/slug`.
fn id_after(url: &str, marker: &str) -> Option<u32> {
    let start = url.find(marker)? + marker.len();
    let digits: String = url[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[derive(Debug, Deserialize)]
struct JikanResponse {
    data: Option<JikanManga>,
}

#[derive(Debug, Deserialize)]
struct JikanManga {
    status: Option<String>,
    title: Option<String>,
    url: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_databases_are_recognized_by_their_link() {
        assert_eq!(
            parse_source_url("https://anilist.co/manga/30642/Apotheosis/"),
            Some(StatusSource::AniList(30642))
        );
        // AniList hands out /media/ links from search as well as /manga/.
        assert_eq!(
            parse_source_url("https://anilist.co/media/30642"),
            Some(StatusSource::AniList(30642))
        );
        assert_eq!(
            parse_source_url("https://myanimelist.net/manga/58784/The_Maxed-out_Player"),
            Some(StatusSource::MyAnimeList(58784))
        );
    }

    /// A link to something else is not an error, just not a status source —
    /// the check run has to survive whatever ends up in that field.
    #[test]
    fn anything_else_is_no_status_source() {
        assert_eq!(parse_source_url("https://example.com/manga/1"), None);
        assert_eq!(parse_source_url("https://anilist.co/user/someone"), None);
        assert_eq!(parse_source_url(""), None);
    }

    #[test]
    fn both_status_vocabularies_map_onto_the_same_meanings() {
        for finished in ["FINISHED", "Finished"] {
            assert_eq!(classify(finished), OriginalStatus::Completed);
        }
        for running in ["RELEASING", "Publishing"] {
            assert_eq!(classify(running), OriginalStatus::Ongoing);
        }
        for paused in ["HIATUS", "On Hiatus"] {
            assert_eq!(classify(paused), OriginalStatus::Hiatus);
        }
        for dropped in ["CANCELLED", "Discontinued"] {
            assert_eq!(classify(dropped), OriginalStatus::Dropped);
        }
    }

    /// "Not yet published" contains "publish"; reading it as "publishing"
    /// would call a work that has not started a running one.
    #[test]
    fn a_work_that_has_not_started_is_not_running() {
        assert_eq!(classify("NOT_YET_RELEASED"), OriginalStatus::Unknown);
        assert_eq!(classify("Not yet published"), OriginalStatus::Unknown);
        assert_eq!(classify(""), OriginalStatus::Unknown);
    }

    #[test]
    fn a_jikan_answer_becomes_an_entry() {
        let body = r#"{"data":{"mal_id":58784,"url":"https://myanimelist.net/manga/58784",
            "title":"The Maxed-out Player","status":"Finished"}}"#;
        let entry = parse_jikan(body, 58784).expect("body should parse");
        assert_eq!(entry.publication, OriginalStatus::Completed);
        assert_eq!(entry.title.as_deref(), Some("The Maxed-out Player"));
    }

    #[test]
    fn an_empty_jikan_answer_is_an_error_not_an_ongoing_series() {
        let result = parse_jikan(r#"{"data":null}"#, 1);
        assert!(matches!(result, Err(FeroError::ExternalApi(_))));
    }
}
