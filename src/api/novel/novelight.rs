//! # api::novel::novelight
//!
//! Novelight adapter (`novelight.net`) — **best effort**.
//!
//! The book page (`/book/<slug>`) lists only the newest ~50 chapters; older
//! entries load through a JavaScript pagination widget whose endpoint is not
//! reachable via plain HTTP. This adapter therefore tracks the chapters that
//! are visible — new releases appear there first, so ongoing reading works,
//! but a full back-catalog download of long novels is not possible.
//!
//! ## Page structure
//! - Book page: title `h1`, cover via `og:image`, description `.description`,
//!   chapters `.chapters a.chapter` (newest first → reversed).
//! - Chapter page: content in `.chapter__content` / `#chapter-content`.
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
const CHAPTER_CONTENT_SELECTORS: [&str; 3] = [".chapter__content", "#chapter-content", ".content"];

/// Novelight source adapter (newest chapters only).
pub struct NovelightSource;

impl NovelSource for NovelightSource {
    fn id(&self) -> &'static str {
        "novelight"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        parse_book_page(&final_url, &body)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "Novelight-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&content),
        })
    }
}

/// Parses a Novelight book page (visible chapter window only).
fn parse_book_page(page_url: &str, body: &str) -> Result<NovelInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, "h1").ok_or_else(|| {
        VaultError::ExternalApi(format!("Novelight-Titel nicht gefunden: {page_url}"))
    })?;

    let chapter_selector = Selector::parse(".chapters a.chapter, a.chapter")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;
    let title_selector = Selector::parse(".title").ok();

    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&chapter_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        // Prefer the dedicated title element; the raw link text also carries
        // date/author noise.
        let text = title_selector
            .as_ref()
            .and_then(|selector| link.select(selector).next())
            .map(|element| element.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| link.text().collect::<Vec<_>>().join(" "));
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let url = absolutize(page_url, href);
        if seen.insert(url.clone()) {
            chapters.push(ChapterRef { title: text, url });
        }
    }

    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Keine Kapitel auf der Novelight-Seite gefunden: {page_url}"
        )));
    }
    // Listed newest-first; adapters must return oldest-first.
    chapters.reverse();

    Ok(NovelInfo {
        title,
        author: first_text(&html, ".author"),
        cover_url: super::og_image(&html).map(|src| absolutize(page_url, &src)),
        description: first_text(&html, ".description"),
        completed_hint: None,
        genres: Vec::new(),
        tags: collect_texts(&html, ".tag"),
        chapters,
    })
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let text = element.text().collect::<Vec<_>>().join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn collect_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|element| {
            let text = element.text().collect::<Vec<_>>().join(" ");
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|text| !text.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK_PAGE: &str = r#"
    <html><head><meta property="og:image" content="/covers/shadow.jpg"/></head><body>
      <h1>Shadow Slave</h1>
      <div class="description">Growing up in poverty…</div>
      <div class="chapters">
        <a href="/book/chapter/323107" class="chapter ">
          <div class="title">3090 chapter - <span>Born Into an Endless War</span></div>
          <div class="chapter-info"><span class="date">10.07.2026</span></div>
        </a>
        <a href="/book/chapter/323106" class="chapter ">
          <div class="title">3089 chapter - <span>Two Titans</span></div>
        </a>
      </div>
    </body></html>"#;

    #[test]
    fn parses_visible_chapters_oldest_first() {
        let info = parse_book_page("https://novelight.net/book/shadow-slave-novel", BOOK_PAGE)
            .expect("page should parse");
        assert_eq!(info.title, "Shadow Slave");
        assert_eq!(info.chapters.len(), 2);
        // Reversed: the lower chapter number comes first.
        assert!(info.chapters[0].title.starts_with("3089"));
        assert_eq!(
            info.chapters[1].url,
            "https://novelight.net/book/chapter/323107"
        );
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://novelight.net/covers/shadow.jpg")
        );
    }
}
