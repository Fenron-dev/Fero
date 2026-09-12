//! # api::manga::chikari
//!
//! Chikari manga adapter (`chikari.moe`).
//!
//! Chikari serves novels and comics from separate JSON APIs.  Manga, manhwa
//! and manhua series live below `/api/series`; a series response supplies the
//! metadata, the chapters endpoint supplies the table of contents, and the
//! chapter endpoint returns the ordered CDN page URLs.
//!
//! The adapter deliberately keeps the public series URL in each chapter
//! reference.  The API URLs are derived only when a chapter is downloaded, so
//! subscriptions remain readable and stable if the site's API changes.

use serde::Deserialize;

use super::{MangaChapterRef, MangaInfo, MangaSource, PageImage};
use crate::api::novel::PoliteClient;
use crate::api::release_date;
use crate::core::subscription::unix_now;
use crate::error::{FeroError, Result};

const API_ROOT: &str = "https://chikari.moe/api/series";
const SITE_ROOT: &str = "https://chikari.moe";
const PAGE_SIZE: usize = 500;
const MAX_TOC_PAGES: usize = 1_000;

/// Chikari's series/API adapter.
pub struct ChikariSource;

impl MangaSource for ChikariSource {
    fn id(&self) -> &'static str {
        "chikari"
    }

    fn fetch_series_info(&self, client: &PoliteClient, url: &str) -> Result<MangaInfo> {
        let slug = series_slug(url).ok_or_else(|| {
            FeroError::ExternalApi(format!("Chikari-URL ohne Serien-Slug: {url}"))
        })?;
        let api_url = format!("{API_ROOT}/{slug}");
        let (_final_url, body) = client.get_text(&api_url)?;
        let detail: SeriesDetail = parse_json(&body, "Seriendetails", &api_url)?;

        let mut chapters = Vec::new();
        let mut offset = 0usize;
        for _ in 0..MAX_TOC_PAGES {
            let list_url =
                format!("{API_ROOT}/{slug}/chapters?order=asc&limit={PAGE_SIZE}&offset={offset}");
            let (_final_url, body) = client.get_text(&list_url)?;
            let page: ChapterPage = parse_json(&body, "Kapitelliste", &list_url)?;
            let count = page.items.len();
            chapters.extend(page.items.into_iter().map(|chapter| {
                let number = format_chapter_number(chapter.number);
                let title = if chapter.title.trim().is_empty() {
                    format!("Chapter {number}")
                } else {
                    chapter.title
                };
                MangaChapterRef {
                    title,
                    url: format!("{SITE_ROOT}/series/{slug}/{number}"),
                    volume: non_empty(chapter.volume),
                    number: Some(number),
                }
            }));
            offset += count;
            if count == 0 || offset >= page.total {
                break;
            }
        }

        if chapters.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Kapitel bei Chikari gefunden: {url}"
            )));
        }

        let latest_release_unix = detail
            .last_chapter_at
            .as_deref()
            .and_then(|date| release_date::parse_release(date, unix_now()));
        let author = detail
            .authors
            .iter()
            .find(|author| author.role.eq_ignore_ascii_case("author"))
            .or_else(|| detail.authors.first())
            .map(|author| author.name.clone());
        let artist = detail
            .authors
            .iter()
            .find(|author| author.role.eq_ignore_ascii_case("artist"))
            .map(|author| author.name.clone());

        Ok(MangaInfo {
            title: detail.title,
            author,
            artist,
            cover_url: detail.cover_url,
            description: detail.description,
            completed_hint: completed_hint(detail.status.as_deref()),
            latest_release_unix,
            genres: detail.genres.into_iter().map(|value| value.name).collect(),
            tags: detail
                .tags
                .into_iter()
                .filter(|tag| !tag.is_spoiler)
                .map(|tag| tag.name)
                .collect(),
            // Chikari distinguishes regular Japanese manga from manhwa/manhua
            // in `type`; the latter are generally read left-to-right.
            right_to_left: detail.type_name.eq_ignore_ascii_case("manga"),
            chapters,
        })
    }

    fn fetch_chapter_pages(
        &self,
        client: &PoliteClient,
        chapter: &MangaChapterRef,
    ) -> Result<Vec<PageImage>> {
        let (slug, number) = chapter_key(&chapter.url).ok_or_else(|| {
            FeroError::ExternalApi(format!("Ungültige Chikari-Kapitel-URL: {}", chapter.url))
        })?;
        let api_url = format!("{API_ROOT}/{slug}/chapters/{number}");
        let (_final_url, body) = client.get_text(&api_url)?;
        let payload: ChapterPayload = parse_json(&body, "Kapitel", &api_url)?;
        let pages = payload
            .pages
            .into_iter()
            .filter(|url| !url.trim().is_empty())
            .map(PageImage::plain)
            .collect::<Vec<_>>();
        if pages.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Seitenbilder bei Chikari gefunden: {}",
                chapter.url
            )));
        }
        Ok(pages)
    }
}

#[derive(Deserialize)]
struct SeriesDetail {
    title: String,
    #[serde(rename = "type")]
    type_name: String,
    status: Option<String>,
    description: Option<String>,
    cover_url: Option<String>,
    last_chapter_at: Option<String>,
    #[serde(default)]
    authors: Vec<Author>,
    #[serde(default)]
    genres: Vec<NamedValue>,
    #[serde(default)]
    tags: Vec<Tag>,
}

#[derive(Deserialize)]
struct Author {
    name: String,
    #[serde(default)]
    role: String,
}

#[derive(Deserialize)]
struct NamedValue {
    name: String,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
    #[serde(default)]
    is_spoiler: bool,
}

#[derive(Deserialize)]
struct ChapterPage {
    #[serde(default)]
    items: Vec<ApiChapter>,
    total: usize,
}

#[derive(Deserialize)]
struct ApiChapter {
    number: f64,
    #[serde(default)]
    volume: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct ChapterPayload {
    #[serde(default)]
    pages: Vec<String>,
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &str, kind: &str, url: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|error| {
        FeroError::ExternalApi(format!(
            "Chikari-{kind} konnten nicht gelesen werden ({url}): {error}"
        ))
    })
}

fn series_slug(url: &str) -> Option<String> {
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    parts.next()?; // host
    if parts.next()? != "series" {
        return None;
    }
    parts
        .next()
        .filter(|slug| !slug.is_empty())
        .map(str::to_string)
}

fn chapter_key(url: &str) -> Option<(String, String)> {
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    parts.next()?; // host
    if parts.next()? != "series" {
        return None;
    }
    let slug = parts.next()?.to_string();
    let number = parts.next()?.to_string();
    (!slug.is_empty() && !number.is_empty()).then_some((slug, number))
}

fn format_chapter_number(number: f64) -> String {
    number.to_string()
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn completed_hint(status: Option<&str>) -> Option<bool> {
    match status?.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "finished" => Some(true),
        "ongoing" | "releasing" | "publishing" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_series_and_chapter_keys() {
        assert_eq!(
            series_slug("https://chikari.moe/series/delicious-in-dungeon"),
            Some("delicious-in-dungeon".to_string())
        );
        assert_eq!(
            chapter_key("https://chikari.moe/series/a-series/97.5"),
            Some(("a-series".to_string(), "97.5".to_string()))
        );
    }

    #[test]
    fn parses_chapter_list_and_payload() {
        let page: ChapterPage = serde_json::from_str(
            r#"{"items":[{"number":1.0,"volume":"","title":""},{"number":12.5,"volume":"2","title":"Finale"}],"total":2}"#,
        )
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(format_chapter_number(page.items[1].number), "12.5");
        assert_eq!(non_empty(page.items[0].volume.clone()), None);

        let payload: ChapterPayload =
            serde_json::from_str(r#"{"pages":["https://cdn.chikari.moe/series/1/ch/1/000.webp"]}"#)
                .unwrap();
        assert_eq!(payload.pages.len(), 1);
    }

    #[test]
    fn ignores_spoiler_tags_and_resolves_status() {
        let detail: SeriesDetail = serde_json::from_str(
            r#"{"title":"M","type":"manga","status":"completed","authors":[{"name":"A","role":"author"},{"name":"B","role":"artist"}],"tags":[{"name":"safe","is_spoiler":false},{"name":"spoiler","is_spoiler":true}]}"#,
        )
        .unwrap();
        assert_eq!(completed_hint(detail.status.as_deref()), Some(true));
        assert!(detail.type_name.eq_ignore_ascii_case("manga"));
        assert_eq!(detail.tags.iter().filter(|tag| !tag.is_spoiler).count(), 1);
    }
}
