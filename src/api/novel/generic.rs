//! # api::novel::generic
//!
//! Heuristic fallback adapter for arbitrary novel sites (WebToEpub-style).
//!
//! ## Table-of-contents heuristic
//! Collect all same-host links on the page, group them by URL path prefix,
//! keep only links whose text looks like a chapter entry, and pick the
//! largest coherent group in document order.
//!
//! ## Chapter-content heuristic
//! Try a list of common content selectors and pick the match with the most
//! text; this survives most blog/reader themes.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and HTML utilities

use std::collections::HashMap;

use scraper::{Html, Selector};

use super::{
    absolutize, host_of, looks_like_chapter_text, sanitize_to_xhtml, ChapterContent, ChapterRef,
    NovelInfo, NovelSource, PoliteClient,
};
use crate::error::{Result, FeroError};

/// Candidate content selectors for chapter pages.
const CONTENT_CANDIDATES: [&str; 6] = [
    "#chapter-content",
    ".chapter-content",
    ".entry-content",
    "article",
    "#content",
    "main",
];

/// Minimum plain-text length for a fragment to count as chapter content.
/// Shorter matches are usually navigation shells around the real text.
const MIN_CONTENT_TEXT_LEN: usize = 200;

/// Heuristic fallback adapter.
pub struct GenericSource;

impl NovelSource for GenericSource {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        parse_novel_info(&final_url, &body)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let content = extract_best_content(&body).ok_or_else(|| {
            FeroError::ExternalApi(format!(
                "Kapitelinhalt konnte nicht erkannt werden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: content,
        })
    }
}

/// Picks the candidate fragment with the largest sanitized text volume.
///
/// Exposed within the crate so the NovelUpdates radar can reuse it on
/// translator-host pages it redirects to.
pub(crate) fn extract_best_content(body: &str) -> Option<String> {
    let html = Html::parse_document(body);
    let mut best: Option<String> = None;
    let mut best_len = 0usize;

    for raw_selector in CONTENT_CANDIDATES {
        let Ok(selector) = Selector::parse(raw_selector) else {
            continue;
        };
        for element in html.select(&selector) {
            let sanitized = sanitize_to_xhtml(&element.inner_html());
            let text_len = visible_text_len(&sanitized);
            if text_len > best_len {
                best_len = text_len;
                best = Some(sanitized);
            }
        }
    }

    // Readability-style fallback for pages (esp. JS-rendered ones) that use
    // no recognizable content id/class: pick the block element whose direct
    // paragraphs hold the most text.
    if best_len < MIN_CONTENT_TEXT_LEN {
        if let Ok(block_selector) = Selector::parse("div, section, article, main") {
            let para_selector = Selector::parse("p").ok();
            for element in html.select(&block_selector) {
                let para_text: usize = para_selector
                    .as_ref()
                    .map(|sel| {
                        element
                            .select(sel)
                            .map(|p| p.text().map(|t| t.trim().len()).sum::<usize>())
                            .sum()
                    })
                    .unwrap_or(0);
                if para_text > best_len {
                    let sanitized = sanitize_to_xhtml(&element.inner_html());
                    let text_len = visible_text_len(&sanitized);
                    if text_len > best_len {
                        best_len = text_len;
                        best = Some(sanitized);
                    }
                }
            }
        }
    }

    if best_len >= MIN_CONTENT_TEXT_LEN {
        best
    } else {
        None
    }
}

/// Approximates the visible text length of an XHTML fragment.
fn visible_text_len(xhtml: &str) -> usize {
    let mut len = 0usize;
    let mut in_tag = false;
    for ch in xhtml.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag && !ch.is_whitespace() => len += 1,
            _ => {}
        }
    }
    len
}

/// Parses an arbitrary novel overview page via the link-group heuristic.
fn parse_novel_info(page_url: &str, body: &str) -> Result<NovelInfo> {
    let html = Html::parse_document(body);
    let page_host = host_of(page_url).unwrap_or_default();

    let title = first_heading(&html).unwrap_or_else(|| page_url.to_string());

    let link_selector = Selector::parse("a[href]")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    // Group same-host chapter-looking links by their URL path prefix so
    // chapter lists ("/novel/ch-1", "/novel/ch-2") cluster together and
    // scattered nav links ("/about", "/latest") stay isolated.
    let mut groups: HashMap<String, Vec<ChapterRef>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&link_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let text = link.text().collect::<Vec<_>>().join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !looks_like_chapter_text(&text) {
            continue;
        }
        let url = absolutize(page_url, href);
        if host_of(&url).unwrap_or_default() != page_host {
            continue;
        }
        if !seen.insert(url.clone()) {
            continue;
        }
        groups
            .entry(path_prefix(&url))
            .or_default()
            .push(ChapterRef { title: text, url });
    }

    let chapters = groups
        .into_values()
        .max_by_key(|group| group.len())
        .unwrap_or_default();

    if chapters.len() < 2 {
        return Err(FeroError::ExternalApi(format!(
            "Keine Kapitelliste erkannt (generischer Parser): {page_url}"
        )));
    }

    Ok(NovelInfo {
        title,
        author: None,
        cover_url: None,
        description: None,
        completed_hint: None,
        genres: Vec::new(),
        tags: Vec::new(),
        chapters,
    })
}

/// Groups URLs by everything before their last path segment.
fn path_prefix(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = without_query.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(pos) => trimmed[..pos].to_string(),
        None => trimmed.to_string(),
    }
}

fn first_heading(html: &Html) -> Option<String> {
    for raw_selector in ["h1", "title"] {
        let Ok(selector) = Selector::parse(raw_selector) else {
            continue;
        };
        if let Some(element) = html.select(&selector).next() {
            let text = element.text().collect::<Vec<_>>().join(" ");
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TOC_PAGE: &str = r#"
    <html><head><title>Some Novel - Reader</title></head><body>
      <h1>Some Novel</h1>
      <a href="/latest">Latest Updates</a>
      <a href="/novel/some-novel/chapter-1">Chapter 1</a>
      <a href="/novel/some-novel/chapter-2">Chapter 2</a>
      <a href="/novel/some-novel/chapter-3">Chapter 3</a>
      <a href="https://ads.example.net/chapter-1">Chapter 1 (mirror)</a>
    </body></html>"#;

    #[test]
    fn picks_largest_same_host_group() {
        let info = parse_novel_info("https://reader.example.org/novel/some-novel", TOC_PAGE)
            .expect("page should parse");
        assert_eq!(info.title, "Some Novel");
        assert_eq!(info.chapters.len(), 3);
        assert_eq!(
            info.chapters[0].url,
            "https://reader.example.org/novel/some-novel/chapter-1"
        );
    }

    #[test]
    fn rejects_pages_without_chapter_list() {
        let result = parse_novel_info(
            "https://reader.example.org/x",
            "<html><body><h1>Nothing here</h1><a href='/about'>About</a></body></html>",
        );
        assert!(result.is_err());
    }

    #[test]
    fn content_extraction_prefers_longest_candidate() {
        let long_text = "Story sentence. ".repeat(30);
        let page = format!(
            "<html><body><main><p>Short nav text</p></main>\
             <article><p>{long_text}</p></article></body></html>"
        );
        let content = extract_best_content(&page).expect("content should extract");
        assert!(content.contains("Story sentence."));
    }

    #[test]
    fn content_extraction_rejects_too_short() {
        let page = "<html><body><article><p>Tiny.</p></article></body></html>";
        assert!(extract_best_content(page).is_none());
    }
}
