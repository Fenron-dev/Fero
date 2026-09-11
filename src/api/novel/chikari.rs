//! Chikari adapter (`chikari.moe`).
//!
//! The public pages are a client-rendered Svelte application, but the data
//! behind them is available as JSON. Using that API avoids opening the
//! embedded browser for every chapter and gives us the complete, ordered ToC.

use serde::Deserialize;

use super::{ChapterContent, ChapterRef, NovelInfo, NovelSource, PoliteClient};
use crate::error::{FeroError, Result};

const API_ROOT: &str = "https://chikari.moe/api/novels";
const PAGE_SIZE: usize = 500;
const MAX_TOC_PAGES: usize = 1_000;

pub struct ChikariSource;

impl NovelSource for ChikariSource {
    fn id(&self) -> &'static str {
        "chikari"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let slug = novel_slug(url)
            .ok_or_else(|| FeroError::ExternalApi(format!("Chikari-URL ohne Novel-Slug: {url}")))?;
        let (_final_url, body) = client.get_text(&format!("{API_ROOT}/{slug}"))?;
        let detail: NovelDetail = parse_json(&body, "Noveldetails", url)?;

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
                ChapterRef {
                    title: chapter.title,
                    url: format!("https://chikari.moe/novels/{slug}/{number}"),
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

        Ok(NovelInfo {
            title: detail.title,
            author: detail
                .authors
                .iter()
                .find(|author| author.role.eq_ignore_ascii_case("author"))
                .or_else(|| detail.authors.first())
                .map(|author| author.name.clone()),
            cover_url: detail.cover_url,
            description: detail.description,
            completed_hint: completed_hint(detail.status.as_deref()),
            latest_release_unix: None,
            genres: detail.genres.into_iter().map(|genre| genre.name).collect(),
            tags: detail.tags.into_iter().map(|tag| tag.name).collect(),
            chapters,
        })
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (slug, number) = chapter_key(&chapter.url).ok_or_else(|| {
            FeroError::ExternalApi(format!("Ungültige Chikari-Kapitel-URL: {}", chapter.url))
        })?;
        let api_url = format!("{API_ROOT}/{slug}/chapters/{number}/read");
        let (_final_url, body) = client.get_text(&api_url)?;
        let payload: ChapterPayload = parse_json(&body, "Kapitel", &api_url)?;
        if payload.locked {
            let reason = payload
                .lock_reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "Kapitel ist gesperrt".to_string());
            return Err(FeroError::ExternalApi(format!(
                "Chikari-Kapitel nicht verfügbar ({reason}): {}",
                chapter.url
            )));
        }
        let chapter_body = payload.body.unwrap_or_default();
        if chapter_body.trim().is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Leerer Chikari-Kapitelinhalt: {}",
                chapter.url
            )));
        }
        Ok(ChapterContent {
            title: payload
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| chapter.title.clone()),
            xhtml: plain_text_to_xhtml(&chapter_body),
        })
    }
}

#[derive(Deserialize)]
struct NovelDetail {
    title: String,
    status: Option<String>,
    description: Option<String>,
    cover_url: Option<String>,
    #[serde(default)]
    authors: Vec<Author>,
    #[serde(default)]
    genres: Vec<NamedValue>,
    #[serde(default)]
    tags: Vec<NamedValue>,
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
struct ChapterPage {
    #[serde(default)]
    items: Vec<ApiChapter>,
    total: usize,
}

#[derive(Deserialize)]
struct ApiChapter {
    number: f64,
    title: String,
}

#[derive(Deserialize)]
struct ChapterPayload {
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    locked: bool,
    lock_reason: Option<String>,
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &str, kind: &str, url: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|error| {
        FeroError::ExternalApi(format!(
            "Chikari-{kind} konnten nicht gelesen werden ({url}): {error}"
        ))
    })
}

fn novel_slug(url: &str) -> Option<String> {
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    parts.next()?; // host
    if parts.next()? != "novels" {
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
    if parts.next()? != "novels" {
        return None;
    }
    let slug = parts.next()?.to_string();
    let number = parts.next()?.to_string();
    (!slug.is_empty() && !number.is_empty()).then_some((slug, number))
}

fn format_chapter_number(number: f64) -> String {
    number.to_string()
}

fn completed_hint(status: Option<&str>) -> Option<bool> {
    match status?.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "finished" => Some(true),
        "ongoing" | "releasing" | "publishing" => Some(false),
        _ => None,
    }
}

fn plain_text_to_xhtml(body: &str) -> String {
    body.split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| {
            let escaped = crate::core::epub::escape_xml(paragraph).replace('\n', "<br/>");
            format!("<p>{escaped}</p>")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_novel_and_chapter_keys() {
        assert_eq!(
            novel_slug("https://chikari.moe/novels/the-quest-for-immortality"),
            Some("the-quest-for-immortality".to_string())
        );
        assert_eq!(
            chapter_key("https://chikari.moe/novels/a-story/12.5"),
            Some(("a-story".to_string(), "12.5".to_string()))
        );
    }

    #[test]
    fn parses_api_pages_and_formats_fractional_numbers() {
        let page: ChapterPage = serde_json::from_str(
            r#"{"items":[{"number":1.0,"title":"Chapter 1"},{"number":12.5,"title":"Extra"}],"total":2}"#,
        )
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(format_chapter_number(page.items[0].number), "1");
        assert_eq!(format_chapter_number(page.items[1].number), "12.5");
    }

    #[test]
    fn converts_plain_text_to_safe_xhtml() {
        let xhtml = plain_text_to_xhtml("One & two\ncontinued\n\n<ending>");
        assert_eq!(
            xhtml,
            "<p>One &amp; two<br/>continued</p>\n<p>&lt;ending&gt;</p>"
        );
    }
}
