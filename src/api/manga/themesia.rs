//! # api::manga::themesia
//!
//! Adapter for the **Themesia** WordPress theme (`eplister` / `ts_reader`).
//!
//! ## Why a theme adapter instead of a site adapter
//! Like [`super::madara`], this covers a whole family of aggregators rather
//! than a single host; `hentai20.io` is the first of them.  New sites on the
//! same theme need one line in [`super::detect_source`].
//!
//! ## Page model
//! The series page lists chapters in `#chapterlist .eplister li a`, each with a
//! `.chapternum` label.  Chapter pages carry their images twice: inside a
//! `<noscript>` fallback in `#readerarea`, and in the `ts_reader.run({…})`
//! bootstrap JSON.
//!
//! Real `<img>` elements in `#readerarea` are read first, since some sites on
//! this theme render them directly.  The `<noscript>` copy is **not** usable —
//! html5ever treats its content as text, not elements — so sites that only
//! ship the fallback are served by the `ts_reader` payload instead.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and URL helpers

use scraper::{Html, Selector};

use super::{
    extract_chapter_number, extract_volume, tidy_text, MangaChapterRef, MangaInfo, MangaSource,
    PageImage,
};
use crate::api::novel::{absolutize, PoliteClient};
use crate::api::release_date;
use crate::core::subscription::unix_now;
use crate::error::{FeroError, Result};

/// Themesia-theme adapter.
pub struct ThemesiaSource;

impl MangaSource for ThemesiaSource {
    fn id(&self) -> &'static str {
        "themesia"
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
        let urls = parse_chapter_images(&final_url, &body)?;
        Ok(urls
            .into_iter()
            .map(|url| PageImage::with_referer(url, final_url.clone()))
            .collect())
    }
}

/// Parses a Themesia series page: metadata plus the full chapter list.
fn parse_series_page(page_url: &str, body: &str) -> Result<MangaInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, "h1.entry-title")
        .or_else(|| first_text(&html, ".entry-title"))
        .or_else(|| first_text(&html, "h1"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("Themesia: Titel nicht gefunden: {page_url}"))
        })?;

    let chapter_selector = Selector::parse("#chapterlist li a, .eplister li a")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;
    let number_selector = Selector::parse(".chapternum")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;
    let date_selector = Selector::parse(".chapterdate")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    let now = unix_now();
    let mut chapters = Vec::new();
    let mut latest_release_unix: Option<u64> = None;
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&chapter_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let url = absolutize(page_url, href.trim());
        if !seen.insert(url.clone()) {
            continue;
        }
        if let Some(released) = link
            .select(&date_selector)
            .next()
            .and_then(|node| release_date::parse_release(&node.text().collect::<String>(), now))
        {
            latest_release_unix = Some(latest_release_unix.unwrap_or(released).max(released));
        }
        // `.chapternum` holds just the label; the anchor also contains the date.
        let label = link
            .select(&number_selector)
            .next()
            .map(|element| tidy_text(&element.text().collect::<String>()))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| tidy_text(&link.text().collect::<String>()));
        chapters.push(MangaChapterRef {
            number: extract_chapter_number(&label).or_else(|| extract_chapter_number(&url)),
            volume: extract_volume(&label).or_else(|| extract_volume(&url)),
            title: if label.is_empty() { url.clone() } else { label },
            url,
        });
    }

    if chapters.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Themesia: Keine Kapitel gefunden: {page_url}"
        )));
    }
    // The theme lists newest first; adapters must return oldest first.
    chapters.reverse();

    Ok(MangaInfo {
        title,
        author: labeled_value(&html, "author"),
        artist: labeled_value(&html, "artist"),
        cover_url: image_source(&html, ".thumb img, .seriestucon img")
            .map(|src| absolutize(page_url, &src)),
        description: first_text(&html, ".entry-content-single")
            .or_else(|| first_text(&html, "[itemprop='description']")),
        latest_release_unix,
        completed_hint: first_text(&html, ".status, .imptdt i")
            .map(|status| status.to_lowercase().contains("completed")),
        genres: collect_texts(&html, ".mgen a, .seriestugenre a"),
        tags: Vec::new(),
        right_to_left: false,
        chapters,
    })
}

/// Reads the ordered page images from a Themesia chapter page.
fn parse_chapter_images(page_url: &str, body: &str) -> Result<Vec<String>> {
    let html = Html::parse_document(body);
    let selector = Selector::parse("#readerarea img")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    let mut images: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for img in html.select(&selector) {
        let element = img.value();
        let Some(src) = element
            .attr("data-src")
            .or_else(|| element.attr("data-lazy-src"))
            .or_else(|| element.attr("src"))
        else {
            continue;
        };
        let src = src.trim();
        if src.is_empty() {
            continue;
        }
        let url = absolutize(page_url, src);
        if seen.insert(url.clone()) {
            images.push(url);
        }
    }

    if images.is_empty() {
        images = parse_ts_reader_images(body).unwrap_or_default();
    }

    if images.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Themesia: Keine Seitenbilder im Kapitel gefunden: {page_url}"
        )));
    }
    Ok(images)
}

/// Fallback: pulls the first server's image list out of `ts_reader.run({…})`.
///
/// The bootstrap JSON escapes every slash (`https:\/\/…`), so the extracted
/// entries are unescaped before use.
fn parse_ts_reader_images(body: &str) -> Option<Vec<String>> {
    let start = body.find("ts_reader.run(")?;
    let region = &body[start..];
    let images_at = region.find("\"images\"")?;
    let array = &region[images_at..];
    let open = array.find('[')?;
    let close = array.find(']')?;
    if close < open {
        return None;
    }

    let images: Vec<String> = array[open + 1..close]
        .split(',')
        .filter_map(|item| {
            let trimmed = item.trim().trim_matches('"').replace("\\/", "/");
            if trimmed.starts_with("http") {
                Some(trimmed)
            } else {
                None
            }
        })
        .collect();

    if images.is_empty() {
        None
    } else {
        Some(images)
    }
}

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

/// Finds a `<b>Label</b> value` pair in the theme's info table.
fn labeled_value(html: &Html, label: &str) -> Option<String> {
    let selector = Selector::parse(".fmed, .infotable tr, .tsinfo .imptdt").ok()?;
    html.select(&selector).find_map(|row| {
        let text = tidy_text(&row.text().collect::<String>());
        let lower = text.to_lowercase();
        let position = lower.find(label)?;
        let value = text[position + label.len()..]
            .trim_start_matches([':', ' ', '-'])
            .trim();
        if value.is_empty() || value == "-" {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn image_source(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let value = element.value();
    let src = value
        .attr("data-src")
        .or_else(|| value.attr("data-lazy-src"))
        .or_else(|| value.attr("src"))?;
    let src = src.trim();
    if src.is_empty() {
        None
    } else {
        Some(src.to_string())
    }
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
    let text = tidy_text(&html.select(&selector).next()?.text().collect::<String>());
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES_PAGE: &str = r#"
    <html><body>
      <h1 class="entry-title">Taming My Stepsister</h1>
      <div class="thumb"><img src="https://cdn.example/cover.jpg"></div>
      <div class="tsinfo">
        <div class="imptdt">Status <i>Ongoing</i></div>
        <div class="fmed"><b>Author</b><span>Some Author</span></div>
      </div>
      <span class="mgen"><a href="/genres/drama/">Drama</a></span>
      <div id="chapterlist"><ul>
        <li data-num="2"><div class="eph-num"><a href="https://h.example/x-chapter-2/">
          <span class="chapternum">Chapter 2</span><span class="chapterdate">August 10, 2026</span>
        </a></div></li>
        <li data-num="1"><div class="eph-num"><a href="https://h.example/x-chapter-1/">
          <span class="chapternum">Chapter 1</span><span class="chapterdate">August 1, 2026</span>
        </a></div></li>
      </ul></div>
    </body></html>"#;

    #[test]
    fn parses_series_metadata_and_orders_chapters_oldest_first() {
        let info = parse_series_page("https://h.example/manga/x/", SERIES_PAGE)
            .expect("series page should parse");

        assert_eq!(info.title, "Taming My Stepsister");
        assert_eq!(info.genres, vec!["Drama".to_string()]);
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].number.as_deref(), Some("1"));
        assert_eq!(info.chapters[1].number.as_deref(), Some("2"));
        // The date must not leak into the chapter title (it becomes a filename).
        assert_eq!(info.chapters[0].title, "Chapter 1");
    }

    #[test]
    fn reads_images_rendered_directly_into_the_reader() {
        let body = "<html><body><div id=\"readerarea\">\
            <img src=\"https://img.example/001.jpg\" />\
            <img data-src=\"https://img.example/002.jpg\" src=\"placeholder.gif\" />\
            </div></body></html>";

        let images = parse_chapter_images("https://h.example/x-chapter-1/", body)
            .expect("chapter should parse");
        assert_eq!(
            images,
            vec![
                "https://img.example/001.jpg".to_string(),
                "https://img.example/002.jpg".to_string()
            ]
        );
    }

    #[test]
    fn falls_back_to_the_ts_reader_payload() {
        // No markup images at all — only the bootstrap JSON, slashes escaped.
        let body = "<html><body><div id=\"readerarea\"></div><script>ts_reader.run(\
            {\"post_id\":1,\"sources\":[{\"source\":\"Server 1\",\"images\":\
            [\"https:\\/\\/img.example\\/001.jpg\",\"https:\\/\\/img.example\\/002.jpg\"]}]}\
            );</script></body></html>";

        let images = parse_chapter_images("https://h.example/x-chapter-1/", body)
            .expect("chapter should parse from JSON");
        assert_eq!(
            images,
            vec![
                "https://img.example/001.jpg".to_string(),
                "https://img.example/002.jpg".to_string()
            ]
        );
    }

    #[test]
    fn empty_chapter_is_an_error() {
        let error = parse_chapter_images(
            "https://h.example/x-chapter-1/",
            "<html><body><div id=\"readerarea\"></div></body></html>",
        )
        .map(|images| images.len())
        .expect_err("empty chapter should error");
        assert!(error.to_string().contains("Keine Seitenbilder"));
    }
}
