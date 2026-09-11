//! LightNovelPub adapter (`lightnovelpub.me`).
//!
//! The site exposes forty chapters per `/book/<slug>/<page>` page and guards
//! later requests with Cloudflare, so it is fetched through Fero's manual
//! browser route.

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, og_image, sanitize_to_xhtml, ChapterContent, ChapterRef,
    NovelInfo, NovelSource, PoliteClient,
};
use crate::error::{FeroError, Result};

const MAX_TOC_PAGES: u32 = 1_000;
const CHAPTER_CONTENT_SELECTORS: [&str; 5] = [
    "#chapter-container",
    "#chr-content",
    ".chapter-content",
    "#content",
    ".chapter-body",
];

pub struct LightNovelPubSource;

impl NovelSource for LightNovelPubSource {
    fn id(&self) -> &'static str {
        "lightnovelpub"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let base = novel_base_url(url).ok_or_else(|| {
            FeroError::ExternalApi(format!("LightNovelPub-URL ohne Book-Slug: {url}"))
        })?;
        let (_final_url, body) = client.get_text(&base)?;
        let html = Html::parse_document(&body);
        let mut info = parse_novel_page(&base, &html)?;
        let last_page = last_page(&html).min(MAX_TOC_PAGES);

        for page in 2..=last_page {
            let page_url = format!("{base}/{page}");
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
        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            FeroError::ExternalApi(format!(
                "LightNovelPub-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&content),
        })
    }
}

fn parse_novel_page(page_url: &str, html: &Html) -> Result<NovelInfo> {
    let title = meta_content(html, "og:novel:novel_name")
        .or_else(|| first_text(html, ".m-desc h1.tit, h1.tit"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("LightNovelPub-Titel nicht gefunden: {page_url}"))
        })?;
    let chapters = parse_chapter_links(page_url, html);
    if chapters.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Keine Kapitel auf der LightNovelPub-Seite gefunden: {page_url}"
        )));
    }

    let status = meta_content(html, "og:novel:status").unwrap_or_default();
    let completed_hint = if status.eq_ignore_ascii_case("completed") {
        Some(true)
    } else if status.eq_ignore_ascii_case("ongoing") {
        Some(false)
    } else {
        None
    };
    let genres = meta_content(html, "og:novel:genre")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|genre| !genre.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(NovelInfo {
        title,
        author: meta_content(html, "og:novel:author"),
        cover_url: og_image(html).map(|url| absolutize(page_url, &url)),
        description: first_text(html, ".m-desc .txt .inner")
            .or_else(|| meta_content(html, "og:description")),
        completed_hint,
        latest_release_unix: None,
        genres,
        tags: Vec::new(),
        chapters,
    })
}

fn parse_chapter_links(base_url: &str, html: &Html) -> Vec<ChapterRef> {
    let Ok(selector) = Selector::parse(".m-newest2 ul.ul-list5 a[href]") else {
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
    let Ok(selector) = Selector::parse("#indexselect option[value]") else {
        return 1;
    };
    html.select(&selector)
        .filter_map(|option| option.value().attr("value")?.parse::<u32>().ok())
        .max()
        .unwrap_or(1)
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

fn meta_content(html: &Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!("meta[property='{property}']")).ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr("content")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()
        .map(|element| normalized_text(&element))
        .filter(|text| !text.is_empty())
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

    const PAGE: &str = r#"
    <html><head>
      <meta property="og:novel:novel_name" content="The Novel's Extra"/>
      <meta property="og:novel:author" content="Jee Gab Song"/>
      <meta property="og:novel:genre" content="ACTION, FANTASY"/>
      <meta property="og:novel:status" content="Completed"/>
      <meta property="og:image" content="/cover.jpg"/>
    </head><body>
      <div class="m-desc"><div class="txt"><div class="inner">A summary.</div></div></div>
      <div class="m-newest2"><ul class="ul-list5">
        <li><a href="/book/the-novels-extra/chapter-1" title="Chapter 0">Chapter 0</a></li>
        <li><a href="/book/the-novels-extra/chapter-2" title="Chapter 1">Chapter 1</a></li>
      </ul><select id="indexselect"><option value="1"></option><option value="13"></option></select></div>
    </body></html>"#;

    #[test]
    fn parses_metadata_chapters_and_page_count() {
        let html = Html::parse_document(PAGE);
        let info = parse_novel_page("https://lightnovelpub.me/book/the-novels-extra", &html)
            .expect("page should parse");
        assert_eq!(info.title, "The Novel's Extra");
        assert_eq!(info.author.as_deref(), Some("Jee Gab Song"));
        assert_eq!(info.completed_hint, Some(true));
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(last_page(&html), 13);
    }

    #[test]
    fn normalizes_book_urls() {
        assert_eq!(
            novel_base_url("https://www.lightnovelpub.me/book/a-story/2?x=1").as_deref(),
            Some("https://www.lightnovelpub.me/book/a-story")
        );
    }
}
