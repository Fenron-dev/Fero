//! # api::novel::royalroad
//!
//! RoyalRoad adapter (`royalroad.com`).
//!
//! ## Page structure
//! - Fiction page: `h1` title, `h4 a` author, `.description` synopsis,
//!   chapter table `table#chapters` with links to `/chapter/` URLs.
//! - Chapter page: content lives in `div.chapter-inner.chapter-content`.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and HTML utilities

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, sanitize_to_xhtml, ChapterContent, ChapterRef, NovelInfo,
    NovelSource, PoliteClient,
};
use crate::error::{Result, VaultError};

/// Content selectors for chapter pages, in priority order.
const CHAPTER_CONTENT_SELECTORS: [&str; 2] =
    ["div.chapter-inner.chapter-content", ".chapter-content"];

/// RoyalRoad source adapter.
pub struct RoyalRoadSource;

impl NovelSource for RoyalRoadSource {
    fn id(&self) -> &'static str {
        "royalroad"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        parse_novel_info(&final_url, &body)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "RoyalRoad-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&content),
        })
    }
}

/// Parses a RoyalRoad fiction overview page.
fn parse_novel_info(page_url: &str, body: &str) -> Result<NovelInfo> {
    let html = Html::parse_document(body);

    let title = select_text(&html, "div.fic-header h1")
        .or_else(|| select_text(&html, "h1"))
        .ok_or_else(|| {
            VaultError::ExternalApi(format!("RoyalRoad-Titel nicht gefunden: {page_url}"))
        })?;
    let author = select_text(&html, "div.fic-header h4 a").or_else(|| select_text(&html, "h4 a"));
    // og:image is the most stable cover source; the header <img> is a
    // fallback for pages without OpenGraph tags.
    let cover_url = super::og_image(&html)
        .or_else(|| select_attr(&html, "div.fic-header img", "src"))
        .or_else(|| select_attr(&html, "img.thumbnail", "src"))
        .map(|src| absolutize(page_url, &src));
    let description = extract_content(&html, &[".description"])
        .map(|fragment| html_fragment_to_text(&fragment))
        .filter(|text| !text.is_empty());

    // "COMPLETED" appears as a status label on finished fictions.
    let completed_hint = extract_content(&html, &["div.fiction-info", "div.fic-header"])
        .map(|info| info.to_uppercase().contains(">COMPLETED<"));

    let chapter_selector = Selector::parse("table#chapters a[href*='/chapter/']")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;
    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&chapter_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let text = element_text(&link);
        if text.is_empty() {
            continue;
        }
        let chapter_url = absolutize(page_url, href);
        // The table repeats each link (title cell + date cell); dedupe by URL.
        if seen.insert(chapter_url.clone()) {
            chapters.push(ChapterRef {
                title: text,
                url: chapter_url,
            });
        }
    }

    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Keine Kapitel auf der RoyalRoad-Seite gefunden: {page_url}"
        )));
    }

    // RoyalRoad exposes fiction tags ("Fantasy", "Time Loop", …) on the page.
    let tags = collect_texts(&html, "a.fiction-tag");

    Ok(NovelInfo {
        title,
        author,
        cover_url,
        description,
        completed_hint,
        genres: Vec::new(),
        tags,
        chapters,
    })
}

/// All matching elements' trimmed text contents.
fn collect_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|element| element_text(&element))
        .filter(|text| !text.is_empty())
        .collect()
}

/// First matching element's trimmed text content.
fn select_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let text = element_text(&element);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// First matching element's attribute value.
fn select_attr(html: &Html, raw_selector: &str, attr: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr(attr)
        .map(str::to_string)
}

/// Collapses an element's text nodes into one whitespace-normalized string.
fn element_text(element: &scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips tags from an HTML fragment, returning readable plain text.
fn html_fragment_to_text(fragment: &str) -> String {
    let parsed = Html::parse_fragment(fragment);
    parsed
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FICTION_PAGE: &str = r#"
    <html><body>
      <div class="fic-header">
        <img src="/covers/full/12345.jpg"/>
        <h1>The Test Fiction</h1>
        <h4>by <a href="/profile/1">AuthorName</a></h4>
      </div>
      <div class="description"><p>A story about <em>tests</em>.</p></div>
      <table id="chapters">
        <tr><td><a href="/fiction/1/f/chapter/100/one">Chapter 1: Start</a></td>
            <td><a href="/fiction/1/f/chapter/100/one">2 years ago</a></td></tr>
        <tr><td><a href="/fiction/1/f/chapter/101/two">Chapter 2: More</a></td>
            <td><a href="/fiction/1/f/chapter/101/two">1 year ago</a></td></tr>
      </table>
    </body></html>"#;

    #[test]
    fn parses_fiction_page() {
        let info = parse_novel_info("https://www.royalroad.com/fiction/1/f", FICTION_PAGE)
            .expect("page should parse");
        assert_eq!(info.title, "The Test Fiction");
        assert_eq!(info.author.as_deref(), Some("AuthorName"));
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://www.royalroad.com/covers/full/12345.jpg")
        );
        assert!(info.description.as_deref().unwrap_or("").contains("tests"));
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Chapter 1: Start");
        assert_eq!(
            info.chapters[0].url,
            "https://www.royalroad.com/fiction/1/f/chapter/100/one"
        );
    }

    #[test]
    fn fails_without_chapters() {
        let result = parse_novel_info(
            "https://www.royalroad.com/fiction/2",
            "<html><body><h1>Empty</h1></body></html>",
        );
        assert!(result.is_err());
    }
}
