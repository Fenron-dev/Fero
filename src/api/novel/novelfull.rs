//! # api::novel::novelfull
//!
//! NovelFull adapter (`novelfull.com` and mirrors like `novelfull.net`).
//!
//! ## Page structure
//! - Novel page: title `h3.title`, cover `.book img`, description
//!   `div.desc-text`, info block `.info` with Author/Genre/Status rows.
//! - Chapter list: `ul.list-chapter li a`, **paginated** 50 chapters per page
//!   via `?page=N` links (`ul.pagination`). All pages are fetched through the
//!   polite client so the full ToC is seen.
//! - Chapter page: content in `div#chapter-content`.
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
const CHAPTER_CONTENT_SELECTORS: [&str; 3] =
    ["div#chapter-content", "#chapter-content", ".chapter-c"];

/// Hard cap on ToC pages so a broken pagination parse can never loop forever.
const MAX_TOC_PAGES: u32 = 200;

/// NovelFull source adapter.
pub struct NovelFullSource;

impl NovelSource for NovelFullSource {
    fn id(&self) -> &'static str {
        "novelfull"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        let html = Html::parse_document(&body);
        let mut info = parse_novel_page(&final_url, &html)?;

        // readnovelfull-style clones expose the full ToC through an AJAX
        // archive endpoint instead of paginated pages; prefer it when present.
        if let Some(novel_id) = data_novel_id(&html) {
            let archive_url = archive_url_for(&final_url, &novel_id);
            if let Ok((_archive_final, archive_body)) = client.get_text(&archive_url) {
                let archive_html = Html::parse_document(&archive_body);
                let archive_chapters = parse_chapter_links(&final_url, &archive_html);
                if archive_chapters.len() > info.chapters.len() {
                    info.chapters = archive_chapters;
                    return Ok(info);
                }
            }
        }

        // The chapter list is paginated; walk the remaining pages.
        let last_page = last_pagination_page(&html).min(MAX_TOC_PAGES);
        for page in 2..=last_page {
            let page_url = with_page_param(&final_url, page);
            let (_page_final, page_body) = client.get_text(&page_url)?;
            let page_html = Html::parse_document(&page_body);
            let mut page_chapters = parse_chapter_links(&final_url, &page_html);
            if page_chapters.is_empty() {
                break; // Ran past the last real page.
            }
            info.chapters.append(&mut page_chapters);
        }

        // Pagination pages can overlap at the edges; dedupe by URL, keep order.
        let mut seen = std::collections::HashSet::new();
        info.chapters
            .retain(|chapter| seen.insert(chapter.url.clone()));

        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "NovelFull-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&content),
        })
    }
}

/// Parses the novel page: metadata plus the first chapter-list page.
fn parse_novel_page(page_url: &str, html: &Html) -> Result<NovelInfo> {
    let title = first_text(html, "h3.title").ok_or_else(|| {
        VaultError::ExternalApi(format!("NovelFull-Titel nicht gefunden: {page_url}"))
    })?;

    let cover_url = first_attr(html, ".book img", "src")
        .or_else(|| super::og_image(html))
        .map(|src| absolutize(page_url, &src));
    let description = first_text(html, "div.desc-text");
    let author = info_row_values(html, "Author:").into_iter().next();
    let genres = info_row_values(html, "Genre:");
    let status = info_row_values(html, "Status:").into_iter().next();
    let completed_hint = status.map(|status| status.to_lowercase().contains("completed"));

    let chapters = parse_chapter_links(page_url, html);
    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Keine Kapitel auf der NovelFull-Seite gefunden: {page_url}"
        )));
    }

    Ok(NovelInfo {
        title,
        author,
        cover_url,
        description,
        completed_hint,
        genres,
        tags: Vec::new(),
        chapters,
    })
}

/// Extracts the chapter links of one list page, in document order.
fn parse_chapter_links(base_url: &str, html: &Html) -> Vec<ChapterRef> {
    let Ok(selector) = Selector::parse("ul.list-chapter li a[href]") else {
        return Vec::new();
    };
    let mut chapters = Vec::new();
    for link in html.select(&selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        // The title attribute holds the clean chapter name; the text nests
        // inside span.chapter-text with the same value as fallback.
        let title = link
            .value()
            .attr("title")
            .map(str::to_string)
            .unwrap_or_else(|| {
                let text = link.text().collect::<Vec<_>>().join(" ");
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            });
        if title.is_empty() {
            continue;
        }
        chapters.push(ChapterRef {
            title,
            url: absolutize(base_url, href),
        });
    }
    chapters
}

/// Reads the `data-novel-id` attribute used by readnovelfull-style clones.
fn data_novel_id(html: &Html) -> Option<String> {
    let selector = Selector::parse("[data-novel-id]").ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr("data-novel-id")
        .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// AJAX chapter-archive URL on the novel's host.
fn archive_url_for(novel_url: &str, novel_id: &str) -> String {
    let host = super::host_of(novel_url).unwrap_or_default();
    let scheme = novel_url.split("://").next().unwrap_or("https");
    format!("{scheme}://{host}/ajax/chapter-archive?novelId={novel_id}")
}

/// Finds the highest `?page=N` value in the pagination bar (1 if absent).
fn last_pagination_page(html: &Html) -> u32 {
    let Ok(selector) = Selector::parse("ul.pagination a[href]") else {
        return 1;
    };
    let mut last = 1u32;
    for link in html.select(&selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if let Some(page) = page_param(href) {
            last = last.max(page);
        }
    }
    last
}

/// Extracts the `page` query parameter from an URL, if present.
fn page_param(url: &str) -> Option<u32> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "page" {
                return value.parse().ok();
            }
        }
    }
    None
}

/// Appends/replaces the `page` parameter on the novel URL.
fn with_page_param(url: &str, page: u32) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    format!("{base}?page={page}")
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

fn first_attr(html: &Html, raw_selector: &str, attr: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr(attr)
        .map(str::to_string)
}

/// Values of one `.info` row identified by its `<h3>` label (e.g. "Genre:").
///
/// NovelFull renders rows as `<div><h3>Genre:</h3><a>…</a>, <a>…</a></div>`.
fn info_row_values(html: &Html, label: &str) -> Vec<String> {
    let Ok(row_selector) = Selector::parse(".info div") else {
        return Vec::new();
    };
    let Ok(label_selector) = Selector::parse("h3") else {
        return Vec::new();
    };
    let Ok(link_selector) = Selector::parse("a") else {
        return Vec::new();
    };

    for row in html.select(&row_selector) {
        let matches_label = row
            .select(&label_selector)
            .next()
            .map(|h3| h3.text().collect::<String>().trim() == label)
            .unwrap_or(false);
        if !matches_label {
            continue;
        }
        return row
            .select(&link_selector)
            .map(|link| {
                let text = link.text().collect::<Vec<_>>().join(" ");
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .filter(|text| !text.is_empty())
            .collect();
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const NOVEL_PAGE: &str = r#"
    <html><body>
      <div class="book"><img src="/uploads/thumbs/otome-cover.jpg" alt="cover"/></div>
      <h3 class="title">The World of Otome Games is Tough For Mobs</h3>
      <div class="info">
        <div><h3>Author:</h3><a href="/author/x">Mishima Yomu</a></div>
        <div><h3>Genre:</h3><a href="/genre/Harem">Harem</a>, <a href="/genre/Comedy">Comedy</a></div>
        <div><h3>Status:</h3><a href="/status/Completed">Completed</a></div>
      </div>
      <div class="desc-text"><p>Leon reincarnates into an otome game.</p></div>
      <div id="list-chapter">
        <ul class="list-chapter">
          <li><a href="/the-world-of-otome/chapter-0-prologue.html" title="Chapter 0 - prologue">
            <span class="chapter-text">Chapter 0 - prologue</span></a></li>
          <li><a href="/the-world-of-otome/chapter-1.html" title="Chapter 1">
            <span class="chapter-text">Chapter 1</span></a></li>
        </ul>
        <ul class="pagination pagination-sm">
          <li class="active"><a href="/novel.html" data-page="0">1</a></li>
          <li><a href="/novel.html?page=2" data-page="1">2</a></li>
          <li class="last"><a href="/novel.html?page=5" data-page="4">Last</a></li>
        </ul>
      </div>
    </body></html>"#;

    #[test]
    fn parses_novel_page_with_metadata() {
        let html = Html::parse_document(NOVEL_PAGE);
        let info = parse_novel_page("https://novelfull.com/the-world-of-otome.html", &html)
            .expect("page should parse");
        assert_eq!(info.title, "The World of Otome Games is Tough For Mobs");
        assert_eq!(info.author.as_deref(), Some("Mishima Yomu"));
        assert_eq!(info.genres, vec!["Harem".to_string(), "Comedy".to_string()]);
        assert_eq!(info.completed_hint, Some(true));
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://novelfull.com/uploads/thumbs/otome-cover.jpg")
        );
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Chapter 0 - prologue");
    }

    #[test]
    fn finds_last_pagination_page() {
        let html = Html::parse_document(NOVEL_PAGE);
        assert_eq!(last_pagination_page(&html), 5);
    }

    #[test]
    fn page_param_helpers() {
        assert_eq!(page_param("/novel.html?page=5"), Some(5));
        assert_eq!(page_param("/novel.html?per-page=50&page=3"), Some(3));
        assert_eq!(page_param("/novel.html"), None);
        assert_eq!(
            with_page_param("https://novelfull.com/n.html?page=1", 4),
            "https://novelfull.com/n.html?page=4"
        );
    }
}
