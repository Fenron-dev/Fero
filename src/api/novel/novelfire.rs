//! NovelFire adapter (`novelfire.net`).
//!
//! NovelFire's overview and chapter pages use the same engine as its current
//! public reader. The full ToC lives under `/book/<slug>/chapters?page=N` and
//! contains one hundred entries per page. Chapter text is inside the
//! `article#chapter-article #content` element. Cloudflare-protected pages are
//! read through Fero's manual embedded-browser route.

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, og_image, sanitize_to_xhtml, ChapterContent, ChapterRef,
    NovelInfo, NovelSource, PoliteClient,
};
use crate::error::{FeroError, Result};

const MAX_TOC_PAGES: u32 = 1_000;
const CHAPTER_CONTENT_SELECTORS: [&str; 6] = [
    "article#chapter-article #content",
    "#chapter-article #content",
    "#content",
    ".chapter-content",
    ".chapter-container",
    "main",
];

pub struct NovelFireSource;

impl NovelSource for NovelFireSource {
    fn id(&self) -> &'static str {
        "novelfire"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let base = novel_base_url(url).ok_or_else(|| {
            FeroError::ExternalApi(format!("NovelFire-URL ohne Book-Slug: {url}"))
        })?;
        let (_final_url, body) = client.get_text(&base)?;
        let html = Html::parse_document(&body);
        let mut info = parse_novel_page(&base, &html)?;

        let chapters_base = format!("{base}/chapters");
        let (_final_url, body) = client.get_text(&chapters_base)?;
        let html = Html::parse_document(&body);
        info.chapters = parse_chapter_links(&base, &html);
        let last_page = last_page(&html).min(MAX_TOC_PAGES);
        for page in 2..=last_page {
            let page_url = format!("{chapters_base}?page={page}");
            let (_final_url, body) = client.get_text(&page_url)?;
            let html = Html::parse_document(&body);
            let mut page_chapters = parse_chapter_links(&base, &html);
            if page_chapters.is_empty() {
                break;
            }
            info.chapters.append(&mut page_chapters);
        }

        let mut seen = std::collections::HashSet::new();
        info.chapters
            .retain(|chapter| seen.insert(chapter.url.clone()));
        if info.chapters.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Kapitel auf der NovelFire-Seite gefunden: {base}"
            )));
        }
        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_chapter_content(&html).ok_or_else(|| {
            FeroError::ExternalApi(format!(
                "NovelFire-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: content,
        })
    }
}

/// Extracts the reader body without accidentally accepting the surrounding
/// navigation shell. NovelFire currently uses `article#chapter-article #content`;
/// the broader selectors keep older and mirror layouts working. Empty shells
/// are ignored so a successful HTTP response cannot become an empty chapter.
fn extract_chapter_content(html: &Html) -> Option<String> {
    for raw_selector in CHAPTER_CONTENT_SELECTORS {
        let Some(raw) = extract_content(html, &[raw_selector]) else {
            continue;
        };
        let sanitized = sanitize_to_xhtml(&raw);
        if has_visible_text(&sanitized) {
            return Some(sanitized);
        }
    }
    None
}

fn has_visible_text(xhtml: &str) -> bool {
    Html::parse_fragment(xhtml)
        .root_element()
        .text()
        .any(|text| !text.trim().is_empty())
}

fn parse_novel_page(page_url: &str, html: &Html) -> Result<NovelInfo> {
    let title = first_text(html, ".novel-title")
        .or_else(|| first_text(html, "h1"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("NovelFire-Titel nicht gefunden: {page_url}"))
        })?;
    let status = first_text(html, ".header-stats .ongoing, .header-stats .completed")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let completed_hint = if status.contains("completed") {
        Some(true)
    } else if status.contains("ongoing") {
        Some(false)
    } else {
        None
    };

    Ok(NovelInfo {
        title,
        author: first_text(html, ".author .property-item span, .author .property-item"),
        cover_url: first_attr(html, ".cover img", "data-src")
            .or_else(|| first_attr(html, ".cover img", "src"))
            .or_else(|| og_image(html))
            .map(|url| absolutize(page_url, &url)),
        description: first_text(html, ".summary .content"),
        completed_hint,
        latest_release_unix: None,
        genres: collect_texts(html, ".categories .property-item"),
        tags: Vec::new(),
        chapters: Vec::new(),
    })
}

fn parse_chapter_links(base_url: &str, html: &Html) -> Vec<ChapterRef> {
    let Ok(selector) = Selector::parse(".chapter-list li a[href]") else {
        return Vec::new();
    };
    html.select(&selector)
        .filter_map(|link| {
            let href = link.value().attr("href")?;
            let title = link
                .value()
                .attr("title")
                .map(str::to_string)
                .unwrap_or_else(|| normalized_text(&link));
            (!title.is_empty()).then(|| ChapterRef {
                title,
                url: absolutize(base_url, href),
            })
        })
        .collect()
}

fn last_page(html: &Html) -> u32 {
    let Ok(selector) = Selector::parse("a[href*='page=']") else {
        return 1;
    };
    let mut last = 1;
    for href in html
        .select(&selector)
        .filter_map(|link| link.value().attr("href"))
    {
        let Some((_, query)) = href.split_once('?') else {
            continue;
        };
        for pair in query.split('&') {
            let Some(("page", value)) = pair.split_once('=') else {
                continue;
            };
            if let Ok(page) = value.parse::<u32>() {
                last = last.max(page);
            }
        }
    }
    last
}

fn novel_base_url(url: &str) -> Option<String> {
    let scheme = url.split_once("://")?.0;
    let host = super::host_of(url)?;
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    parts.next()?;
    if parts.next()? != "book" {
        return None;
    }
    let slug = parts.next()?.trim();
    (!slug.is_empty()).then(|| format!("{scheme}://{host}/book/{slug}"))
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()
        .map(|element| normalized_text(&element))
        .filter(|text| !text.is_empty())
}

fn collect_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|element| normalized_text(&element))
        .filter(|text| !text.is_empty())
        .collect()
}

fn first_attr(html: &Html, raw_selector: &str, attr: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr(attr)
        .map(str::to_string)
}

fn normalized_text(element: &scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTERS_PAGE: &str = r#"
    <html><body><ul class="chapter-list">
      <li><a href="/book/the-monarch/chapter-1" title="Chapter 1: Warning">one</a></li>
      <li><a href="/book/the-monarch/chapter-2">Chapter 2: The beginning</a></li>
    </ul><nav><a href="/book/the-monarch/chapters?page=2">2</a>
      <a href="/book/the-monarch/chapters?page=7">Last</a></nav></body></html>"#;

    #[test]
    fn parses_chapters_and_pagination() {
        let html = Html::parse_document(CHAPTERS_PAGE);
        let chapters = parse_chapter_links("https://novelfire.net/book/the-monarch", &html);
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].title, "Chapter 1: Warning");
        assert_eq!(
            chapters[1].url,
            "https://novelfire.net/book/the-monarch/chapter-2"
        );
        assert_eq!(last_page(&html), 7);
    }

    #[test]
    fn parses_overview_metadata() {
        let html = Html::parse_document(
            r#"<html><body><h1 class="novel-title">The Monarch</h1>
            <figure class="cover"><img data-src="/cover.webp"/></figure>
            <div class="header-stats"><strong class="ongoing">Ongoing</strong></div>
            <div class="author"><span class="property-item"><span>N. Francis</span></span></div>
            <div class="categories"><span class="property-item">Fantasy</span></div>
            <div class="summary"><div class="content">A long journey.</div></div></body></html>"#,
        );
        let info = parse_novel_page("https://novelfire.net/book/the-monarch", &html).unwrap();
        assert_eq!(info.title, "The Monarch");
        assert_eq!(info.author.as_deref(), Some("N. Francis"));
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(info.genres, vec!["Fantasy".to_string()]);
    }

    #[test]
    fn extracts_reader_body_from_current_layout() {
        let html = Html::parse_document(
            r#"<main><article id="chapter-article">
              <nav>Previous Chapter · Next Chapter</nav>
              <div id="content" class="clearfix font_default">
                <p>The actual NovelFire chapter text.</p>
              </div>
            </article></main>"#,
        );
        let content = extract_chapter_content(&html).expect("reader body should extract");
        assert!(content.contains("The actual NovelFire chapter text."));
        assert!(!content.contains("Previous Chapter"));
    }

    #[test]
    fn ignores_empty_reader_shells() {
        let html = Html::parse_document(
            r#"<article id="chapter-article"><div id="content"><div></div></div></article>"#,
        );
        assert!(extract_chapter_content(&html).is_none());
    }
}
