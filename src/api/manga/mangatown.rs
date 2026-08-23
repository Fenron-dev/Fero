//! # api::manga::mangatown
//!
//! MangaTown adapter (`mangatown.com`).
//!
//! ## Page model
//! One HTML page per manga page, linked from a `.page_select` dropdown that
//! lists every page of the chapter.  The page image sits in `<img id="image">`
//! with a plain `src` — no JavaScript, no obfuscation — which makes this the
//! simplest of the manga adapters and the reference for new ones.
//!
//! ## Chapter list
//! The series page carries the full chapter list twice: as `<li>` links and in
//! a `.chapter_select` dropdown.  The dropdown is preferred because its option
//! labels contain the volume and chapter number (`Vol.01 Ch.001 Title`), which
//! the plain links do not.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and URL helpers

use scraper::{Html, Selector};

use super::{
    extract_chapter_number, extract_volume, tidy_text, MangaChapterRef, MangaInfo, MangaSource,
    PageImage,
};
use crate::api::novel::{absolutize, PoliteClient};
use crate::error::{FeroError, Result};

/// MangaTown adapter.
pub struct MangaTownSource;

impl MangaSource for MangaTownSource {
    fn id(&self) -> &'static str {
        "mangatown"
    }

    fn fetch_series_info(&self, client: &PoliteClient, url: &str) -> Result<MangaInfo> {
        let (final_url, body) = client.get_text(url)?;
        parse_series_page(&final_url, &body)
    }

    fn fetch_chapter_pages(
        &self,
        client: &PoliteClient,
        chapter: &MangaChapterRef,
    ) -> Result<Vec<PageImage>> {
        let (final_url, body) = client.get_text(&chapter.url)?;
        let page_urls = parse_page_urls(&final_url, &body)?;

        let mut images = Vec::with_capacity(page_urls.len());
        // The first page's image is already in the response we hold.
        if let Some(src) = parse_page_image(&final_url, &body) {
            images.push(PageImage::plain(src));
        }
        for page_url in page_urls.iter().skip(1) {
            let (page_final_url, page_body) = client.get_text(page_url)?;
            let Some(src) = parse_page_image(&page_final_url, &page_body) else {
                return Err(FeroError::ExternalApi(format!(
                    "Seitenbild nicht gefunden: {page_url}"
                )));
            };
            images.push(PageImage::plain(src));
        }

        if images.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Seiten im Kapitel gefunden: {}",
                chapter.url
            )));
        }
        Ok(images)
    }
}

/// Parses a series page: metadata plus the full chapter list.
fn parse_series_page(page_url: &str, body: &str) -> Result<MangaInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, "h1.title-top")
        .or_else(|| first_text(&html, ".detail_info h1"))
        .or_else(|| first_text(&html, "h1"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("MangaTown-Titel nicht gefunden: {page_url}"))
        })?;

    let mut chapters = parse_chapter_options(page_url, &html);
    if chapters.is_empty() {
        chapters = parse_chapter_links(page_url, &html);
    }
    if chapters.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Keine Kapitel auf der MangaTown-Seite gefunden: {page_url}"
        )));
    }
    // Both sources list newest first; adapters must return oldest first.
    chapters.reverse();

    let detail = collect_detail_text(&html);
    Ok(MangaInfo {
        title,
        author: labeled_value(&detail, "author"),
        artist: labeled_value(&detail, "artist"),
        cover_url: first_attr(&html, ".detail_info img", "src")
            .map(|src| absolutize(page_url, &src)),
        description: first_text(&html, "#show")
            .or_else(|| first_text(&html, ".detail_info .summary"))
            .map(|text| text.trim_end_matches("Show less").trim().to_string()),
        completed_hint: labeled_value(&detail, "status")
            .map(|status| status.to_lowercase().contains("completed")),
        genres: collect_texts(&html, ".detail_info li a[href*='/directory/']"),
        tags: Vec::new(),
        // MangaTown hosts Japanese manga; scanlations keep the original
        // right-to-left page order.
        right_to_left: true,
        chapters,
    })
}

/// Reads the chapter list from the `.chapter_select` dropdown.
fn parse_chapter_options(page_url: &str, html: &Html) -> Vec<MangaChapterRef> {
    let Ok(selector) = Selector::parse("select.chapter_select option") else {
        return Vec::new();
    };
    html.select(&selector)
        .filter_map(|option| {
            let href = option.value().attr("value")?;
            let label = tidy_text(&option.text().collect::<String>());
            if href.is_empty() {
                return None;
            }
            let url = absolutize(page_url, href);
            Some(MangaChapterRef {
                number: extract_chapter_number(&label).or_else(|| extract_chapter_number(&url)),
                volume: extract_volume(&label).or_else(|| extract_volume(&url)),
                title: if label.is_empty() { url.clone() } else { label },
                url,
            })
        })
        .collect()
}

/// Fallback: reads the chapter list from the plain `<li>` links.
fn parse_chapter_links(page_url: &str, html: &Html) -> Vec<MangaChapterRef> {
    let Ok(selector) = Selector::parse("ul.chapter_list a, .chapter_list a") else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    html.select(&selector)
        .filter_map(|link| {
            let href = link.value().attr("href")?;
            let url = absolutize(page_url, href);
            if !seen.insert(url.clone()) {
                return None;
            }
            let label = tidy_text(&link.text().collect::<String>());
            Some(MangaChapterRef {
                number: extract_chapter_number(&label).or_else(|| extract_chapter_number(&url)),
                volume: extract_volume(&url),
                title: if label.is_empty() { url.clone() } else { label },
                url,
            })
        })
        .collect()
}

/// Reads every page URL of a chapter from the `.page_select` dropdown.
fn parse_page_urls(page_url: &str, body: &str) -> Result<Vec<String>> {
    let html = Html::parse_document(body);
    let selector = Selector::parse(".page_select option")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for option in html.select(&selector) {
        let Some(href) = option.value().attr("value") else {
            continue;
        };
        let url = absolutize(page_url, href);
        // The dropdown ends with a "Featured" comments entry that is not a page.
        if url.ends_with("/featured.html") {
            continue;
        }
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }

    if urls.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Seitenliste nicht gefunden: {page_url}"
        )));
    }
    Ok(urls)
}

/// Extracts the page image URL from a reader page.
fn parse_page_image(page_url: &str, body: &str) -> Option<String> {
    let html = Html::parse_document(body);
    let selector = Selector::parse("img#image").ok()?;
    let src = html.select(&selector).next()?.value().attr("src")?;
    Some(absolutize(page_url, src))
}

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

/// Collects the detail block's list items as lowercase `label: value` lines.
fn collect_detail_text(html: &Html) -> Vec<String> {
    let Ok(selector) = Selector::parse(".detail_info li") else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|item| tidy_text(&item.text().collect::<String>()))
        .collect()
}

/// Finds the value of a `Label: value` detail line.
fn labeled_value(lines: &[String], label: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        let lower = line.to_lowercase();
        let position = lower.find(label)?;
        let after = &line[position + label.len()..];
        let value = after.trim_start_matches([':', '(', 's', ')', ' ']).trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn collect_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|element| tidy_text(&element.text().collect::<String>()))
        .filter(|text| !text.is_empty())
        .collect()
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let text = tidy_text(&element.text().collect::<String>());
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES_PAGE: &str = r#"
    <html><body>
      <h1 class="title-top">Barakamon</h1>
      <div class="detail_info clearfix">
        <img src="//cdn.example/cover.jpg">
        <li>Author(s): Yoshino Satsuki</li>
        <li>Artist(s): Yoshino Satsuki</li>
        <li>Status(s): Completed</li>
        <li><a href="/directory/comedy/">Comedy</a></li>
      </div>
      <div id="show">Ein Kalligraf zieht aufs Land.</div>
      <select class="chapter_select">
        <option value="/manga/barakamon/v02/c015/">Vol.02 Ch.015 Neu</option>
        <option value="/manga/barakamon/v01/c002/">Vol.01 Ch.002 </option>
        <option value="/manga/barakamon/v01/c001/">Vol.01 Ch.001 Barakakodon</option>
      </select>
    </body></html>"#;

    const READER_PAGE: &str = r#"
    <html><body>
      <img src="//cdn.example/store/01-001.0/j00.jpg" id="image" alt="page 1">
      <div class="page_select"><select>
        <option value="/manga/barakamon/v01/c001/1.html">1</option>
        <option value="/manga/barakamon/v01/c001/2.html">2</option>
        <option value="/manga/barakamon/v01/c001/featured.html">Featured</option>
      </select></div>
    </body></html>"#;

    #[test]
    fn parses_series_metadata_and_orders_chapters_oldest_first() {
        let info = parse_series_page("https://www.mangatown.com/manga/barakamon/", SERIES_PAGE)
            .expect("series page should parse");

        assert_eq!(info.title, "Barakamon");
        assert_eq!(info.author.as_deref(), Some("Yoshino Satsuki"));
        assert_eq!(info.completed_hint, Some(true));
        assert_eq!(info.genres, vec!["Comedy".to_string()]);
        assert!(info.right_to_left);
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://cdn.example/cover.jpg")
        );

        assert_eq!(info.chapters.len(), 3);
        // Oldest first after the reverse.
        assert_eq!(info.chapters[0].number.as_deref(), Some("1"));
        assert_eq!(info.chapters[0].volume.as_deref(), Some("v1"));
        assert_eq!(
            info.chapters[0].url,
            "https://www.mangatown.com/manga/barakamon/v01/c001/"
        );
        assert_eq!(info.chapters[2].number.as_deref(), Some("15"));
        assert_eq!(info.chapters[2].volume.as_deref(), Some("v2"));
    }

    #[test]
    fn missing_title_is_an_error_not_an_empty_folder_name() {
        // An empty title would become a vault folder name downstream.
        let result = parse_series_page(
            "https://www.mangatown.com/manga/x/",
            "<html><body>Checking your browser…</body></html>",
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_page_urls_and_skips_the_featured_entry() {
        let urls = parse_page_urls(
            "https://www.mangatown.com/manga/barakamon/v01/c001/",
            READER_PAGE,
        )
        .expect("page list should parse");

        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("/c001/1.html"));
        assert!(urls[1].ends_with("/c001/2.html"));
        assert!(!urls.iter().any(|url| url.contains("featured")));
    }

    #[test]
    fn extracts_the_page_image() {
        let src = parse_page_image(
            "https://www.mangatown.com/manga/barakamon/v01/c001/",
            READER_PAGE,
        )
        .expect("image should be found");
        assert_eq!(src, "https://cdn.example/store/01-001.0/j00.jpg");
    }
}
