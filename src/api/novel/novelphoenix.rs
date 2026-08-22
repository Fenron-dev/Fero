//! # api::novel::novelphoenix
//!
//! Adapter for the LightNovelWorld-style engine used by `novelphoenix.com`.
//!
//! ## Page structure (verified 07/2026)
//! - Novel page `/novel/<slug>`: title `h1.novel-title`, cover `og:image`,
//!   description `.summary`, genre links `/genre/...`, status text.
//! - Chapter list `/novel/<slug>/chapters?page=N` (100 per page, `?page=`
//!   pagination), links `/novel/<slug>/chapter-N`.
//! - Chapter page: content in `#chapter-container`.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and HTML utilities

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, og_image, sanitize_to_xhtml, ChapterContent, ChapterRef,
    NovelInfo, NovelSource, PoliteClient,
};
use crate::error::{Result, VaultError};

/// Content selectors for chapter pages, in priority order.
const CHAPTER_CONTENT_SELECTORS: [&str; 3] = ["#chapter-container", ".chapter-content", "#content"];

/// Hard cap on ToC pages so broken pagination can never loop forever.
const MAX_TOC_PAGES: u32 = 200;

/// LightNovelWorld-engine adapter (novelphoenix.com).
pub struct NovelPhoenixSource;

impl NovelSource for NovelPhoenixSource {
    fn id(&self) -> &'static str {
        "novelphoenix"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        let html = Html::parse_document(&body);
        let mut info = parse_novel_page(&final_url, &html)?;

        // Walk the paginated chapter list.
        let base = chapters_base_url(&final_url);
        let mut page = 1u32;
        loop {
            let page_url = format!("{base}?page={page}");
            let (_page_final, page_body) = client.get_text(&page_url)?;
            let page_html = Html::parse_document(&page_body);
            let mut chapters = parse_chapter_links(&final_url, &page_html);
            if chapters.is_empty() {
                break;
            }
            info.chapters.append(&mut chapters);
            if page >= last_pagination_page(&page_html).min(MAX_TOC_PAGES) {
                break;
            }
            page += 1;
        }

        let mut seen = std::collections::HashSet::new();
        info.chapters
            .retain(|chapter| seen.insert(chapter.url.clone()));

        if info.chapters.is_empty() {
            return Err(VaultError::ExternalApi(format!(
                "Keine Kapitel auf der Seite gefunden: {final_url}"
            )));
        }
        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            VaultError::ExternalApi(format!("Kapitelinhalt nicht gefunden: {}", chapter.url))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&content),
        })
    }
}

/// `/novel/<slug>` → `/novel/<slug>/chapters`.
fn chapters_base_url(novel_url: &str) -> String {
    let trimmed = novel_url.split(['?', '#']).next().unwrap_or(novel_url);
    format!("{}/chapters", trimmed.trim_end_matches('/'))
}

fn parse_novel_page(page_url: &str, html: &Html) -> Result<NovelInfo> {
    let title = first_text(html, "h1.novel-title")
        .or_else(|| first_text(html, "h1"))
        .ok_or_else(|| {
            VaultError::ExternalApi(format!("Novel-Titel nicht gefunden: {page_url}"))
        })?;

    // Genre links; nav entries ("Latest Novels" …) don't use /genre/ URLs.
    let genres = collect_link_texts(html, "a[href*='/genre/']");
    let status_text = first_text(html, ".status").unwrap_or_default();
    let completed_hint = if status_text.to_lowercase().contains("completed") {
        Some(true)
    } else if status_text.to_lowercase().contains("ongoing") {
        Some(false)
    } else {
        None
    };

    Ok(NovelInfo {
        title,
        author: first_text(html, ".author a").or_else(|| first_text(html, "[itemprop='author']")),
        cover_url: og_image(html).map(|src| absolutize(page_url, &src)),
        description: first_text(html, ".summary"),
        completed_hint,
        genres,
        tags: Vec::new(),
        chapters: Vec::new(),
    })
}

fn parse_chapter_links(base_url: &str, html: &Html) -> Vec<ChapterRef> {
    let Ok(selector) = Selector::parse(".chapter-list a[href], ul.chapter-list li a") else {
        return Vec::new();
    };
    let mut chapters = collect_chapter_links(base_url, html, &selector);
    if chapters.is_empty() {
        // Fallback: any same-novel chapter link on the list page.
        if let Ok(loose) = Selector::parse("a[href*='/chapter-']") {
            chapters = collect_chapter_links(base_url, html, &loose);
        }
    }
    chapters
}

fn collect_chapter_links(base_url: &str, html: &Html, selector: &Selector) -> Vec<ChapterRef> {
    let mut chapters = Vec::new();
    for link in html.select(selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.contains("/chapter-") {
            continue;
        }
        let text = link.text().collect::<Vec<_>>().join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        chapters.push(ChapterRef {
            title: text,
            url: absolutize(base_url, href),
        });
    }
    chapters
}

fn last_pagination_page(html: &Html) -> u32 {
    let Ok(selector) = Selector::parse("a[href*='page=']") else {
        return 1;
    };
    let mut last = 1u32;
    for link in html.select(&selector) {
        if let Some(href) = link.value().attr("href") {
            if let Some(query) = href.split_once('?').map(|(_, q)| q) {
                for pair in query.split('&') {
                    if let Some(("page", value)) = pair.split_once('=') {
                        if let Ok(page) = value.parse::<u32>() {
                            last = last.max(page);
                        }
                    }
                }
            }
        }
    }
    last
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

fn collect_link_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    html.select(&selector)
        .map(|link| {
            let text = link.text().collect::<Vec<_>>().join(" ");
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|text| !text.is_empty() && seen.insert(text.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTERS_PAGE: &str = r#"
    <html><body><article id="chapter-list-page">
      <ul class="chapter-list">
        <li><a href="/novel/magus-infinite/chapter-1"><span>Chapter 1</span></a></li>
        <li><a href="/novel/magus-infinite/chapter-2"><span>Chapter 2</span></a></li>
      </ul>
      <nav><a href="/novel/magus-infinite/chapters?page=2">2</a>
           <a href="/novel/magus-infinite/chapters?page=3">3</a></nav>
    </article></body></html>"#;

    #[test]
    fn parses_chapter_list_and_pagination() {
        let html = Html::parse_document(CHAPTERS_PAGE);
        let chapters = parse_chapter_links("https://novelphoenix.com/novel/magus-infinite", &html);
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].url,
            "https://novelphoenix.com/novel/magus-infinite/chapter-1"
        );
        assert_eq!(last_pagination_page(&html), 3);
    }

    #[test]
    fn builds_chapters_url() {
        assert_eq!(
            chapters_base_url("https://novelphoenix.com/novel/magus-infinite/"),
            "https://novelphoenix.com/novel/magus-infinite/chapters"
        );
    }

    const NOVEL_PAGE: &str = r#"
    <html><head><meta property="og:image" content="/server-1/magus.jpg"/></head><body>
      <h1 class="novel-title text2row">MAGUS INFINITE</h1>
      <div class="author"><a href="/author/x">BRICKTRADER</a></div>
      <div class="status"><span>Ongoing</span></div>
      <div class="categories"><a href="/genre/fantasy">Fantasy</a><a href="/genre/action">Action</a></div>
      <div class="summary"><p>A mage climbs forever.</p></div>
    </body></html>"#;

    #[test]
    fn parses_novel_metadata() {
        let html = Html::parse_document(NOVEL_PAGE);
        let info = parse_novel_page("https://novelphoenix.com/novel/magus-infinite", &html)
            .expect("should parse");
        assert_eq!(info.title, "MAGUS INFINITE");
        assert_eq!(info.author.as_deref(), Some("BRICKTRADER"));
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(
            info.genres,
            vec!["Fantasy".to_string(), "Action".to_string()]
        );
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://novelphoenix.com/server-1/magus.jpg")
        );
    }
}
