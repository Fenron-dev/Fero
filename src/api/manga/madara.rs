//! # api::manga::madara
//!
//! Adapter for the **Madara** WordPress theme (`wp-manga`).
//!
//! ## Why a theme adapter instead of a site adapter
//! Madara powers a large share of manga aggregators; `mangaread.org` and
//! `manhuaplus.com` are two of them.  Because the theme fixes the markup
//! (`li.wp-manga-chapter`, `.reading-content`, `.summary__content`), one
//! adapter serves every site built on it — adding the next one is a single
//! line in [`super::detect_source`], not a new module.
//!
//! ## Page model
//! The series page carries the full chapter list inline and each chapter page
//! holds every image in `.reading-content`.  Image `src` attributes are padded
//! with newlines and tabs by the theme's template, so they are trimmed before
//! use — an untrimmed URL fails the request.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and URL helpers

use scraper::{Html, Selector};

use super::{
    extract_chapter_number, extract_volume, tidy_text, MangaChapterRef, MangaInfo, MangaSource,
    PageImage,
};
use crate::api::novel::{absolutize, PoliteClient};
use crate::error::{Result, VaultError};

/// Madara-theme adapter.
pub struct MadaraSource;

impl MangaSource for MadaraSource {
    fn id(&self) -> &'static str {
        "madara"
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
        // The image hosts are usually the site itself and check the referer.
        Ok(urls
            .into_iter()
            .map(|url| PageImage::with_referer(url, final_url.clone()))
            .collect())
    }
}

/// Parses a Madara series page: metadata plus the full chapter list.
fn parse_series_page(page_url: &str, body: &str) -> Result<MangaInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, ".post-title h1")
        .or_else(|| first_text(&html, ".post-title h3"))
        .or_else(|| first_text(&html, "h1"))
        .map(strip_title_badges)
        .filter(|title| !title.is_empty())
        .ok_or_else(|| {
            VaultError::ExternalApi(format!("Madara: Titel nicht gefunden: {page_url}"))
        })?;

    let chapter_selector = Selector::parse("li.wp-manga-chapter > a")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;
    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&chapter_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let url = absolutize(page_url, href.trim());
        if !seen.insert(url.clone()) {
            continue;
        }
        let label = tidy_text(&link.text().collect::<String>());
        chapters.push(MangaChapterRef {
            number: extract_chapter_number(&label).or_else(|| extract_chapter_number(&url)),
            volume: extract_volume(&label).or_else(|| extract_volume(&url)),
            title: if label.is_empty() { url.clone() } else { label },
            url,
        });
    }

    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Madara: Keine Kapitel gefunden — die Seite lädt die Liste \
             möglicherweise per AJAX nach: {page_url}"
        )));
    }
    // Madara lists newest first, but several sites repeat the latest chapters
    // in a separate block, so document order alone scrambles the result.
    chapters.reverse();
    super::sort_chapters_by_number(&mut chapters);

    let status = first_text(&html, ".post-status .summary-content").unwrap_or_default();
    Ok(MangaInfo {
        title,
        author: first_text(&html, ".author-content a"),
        artist: first_text(&html, ".artist-content a"),
        cover_url: image_source(&html, ".summary_image img").map(|src| absolutize(page_url, &src)),
        description: first_text(&html, ".summary__content")
            .or_else(|| first_text(&html, ".description-summary")),
        completed_hint: Some(status.to_lowercase().contains("completed")),
        genres: collect_texts(&html, ".genres-content a"),
        tags: Vec::new(),
        // Madara hosts manga, manhua and manhwa alike; manhua/manhwa read
        // left-to-right, so the safe default is the non-mirrored order.
        right_to_left: false,
        chapters,
    })
}

/// Reads the ordered page images from a Madara chapter page.
fn parse_chapter_images(page_url: &str, body: &str) -> Result<Vec<String>> {
    let html = Html::parse_document(body);
    let selector = Selector::parse(".reading-content img")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;

    let images: Vec<String> = html
        .select(&selector)
        .filter_map(|img| {
            let element = img.value();
            // The theme pads `src` with newlines and tabs; lazy-loading
            // variants put the real URL in `data-src`.
            let src = element
                .attr("data-src")
                .or_else(|| element.attr("data-lazy-src"))
                .or_else(|| element.attr("src"))?;
            let src = src.trim();
            if src.is_empty() {
                return None;
            }
            Some(absolutize(page_url, src))
        })
        .collect();

    if images.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Madara: Keine Seitenbilder im Kapitel gefunden: {page_url}"
        )));
    }
    Ok(images)
}

/// Removes the "NEW"/"HOT" badges the theme appends to the title element.
fn strip_title_badges(title: String) -> String {
    let mut cleaned = title.trim().to_string();
    for badge in ["NEW", "HOT", "New", "Hot"] {
        if let Some(stripped) = cleaned.strip_suffix(badge) {
            cleaned = stripped.trim().to_string();
        }
    }
    cleaned
}

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

/// First usable image URL for a selector, preferring lazy-load attributes.
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
      <div class="post-title"><h1>Yakuza Fiancé <span class="manga-title-badges">NEW</span></h1></div>
      <div class="summary_image"><img src="  https://cdn.example/cover.jpg  "></div>
      <div class="author-content"><a href="/author/x/">Asuka Konishi</a></div>
      <div class="artist-content"><a href="/artist/x/">Asuka Konishi</a></div>
      <div class="genres-content"><a href="/genre/drama/">Drama</a><a href="/genre/romance/">Romance</a></div>
      <div class="summary__content">Eine Yakuza-Tochter.</div>
      <div class="post-status"><div class="summary-content">OnGoing</div></div>
      <ul>
        <li class="wp-manga-chapter"><a href="https://x.example/manga/y/chapter-3/">Chapter 3</a></li>
        <li class="wp-manga-chapter"><a href="https://x.example/manga/y/chapter-2/">Chapter 2</a></li>
        <li class="wp-manga-chapter"><a href="https://x.example/manga/y/chapter-1/">Chapter 1</a></li>
      </ul>
    </body></html>"#;

    // The theme really does emit padded `src` values like this.
    const CHAPTER_PAGE: &str = "<html><body><div class=\"reading-content\">\
        <div class=\"page-break\"><img id=\"image-0\" src=\"\n\t\thttps://cdn.example/1.jpeg\" \
        class=\"wp-manga-chapter-img\"></div>\
        <div class=\"page-break\"><img id=\"image-1\" src=\"\n\t\thttps://cdn.example/2.jpeg\" \
        class=\"wp-manga-chapter-img\"></div></div></body></html>";

    #[test]
    fn parses_series_metadata_and_orders_chapters_oldest_first() {
        let info = parse_series_page("https://x.example/manga/y/", SERIES_PAGE)
            .expect("series page should parse");

        assert_eq!(info.title, "Yakuza Fiancé");
        assert_eq!(info.author.as_deref(), Some("Asuka Konishi"));
        assert_eq!(info.artist.as_deref(), Some("Asuka Konishi"));
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(
            info.genres,
            vec!["Drama".to_string(), "Romance".to_string()]
        );
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://cdn.example/cover.jpg")
        );

        assert_eq!(info.chapters.len(), 3);
        assert_eq!(info.chapters[0].number.as_deref(), Some("1"));
        assert_eq!(info.chapters[2].number.as_deref(), Some("3"));
    }

    #[test]
    fn image_urls_are_trimmed() {
        // An untrimmed src carries newlines and tabs and fails the request.
        let images = parse_chapter_images("https://x.example/manga/y/chapter-1/", CHAPTER_PAGE)
            .expect("chapter should parse");

        assert_eq!(images.len(), 2);
        assert_eq!(images[0], "https://cdn.example/1.jpeg");
        assert!(!images.iter().any(|url| url.contains(char::is_whitespace)));
    }

    #[test]
    fn ajax_only_chapter_list_reports_why() {
        let error = parse_series_page(
            "https://x.example/manga/y/",
            "<html><body><div class=\"post-title\"><h1>T</h1></div></body></html>",
        )
        .map(|info| info.title)
        .expect_err("missing chapters should error");
        assert!(error.to_string().contains("AJAX"));
    }

    #[test]
    fn strips_title_badges() {
        assert_eq!(
            strip_title_badges("Martial Peak NEW".to_string()),
            "Martial Peak"
        );
        assert_eq!(strip_title_badges("Renew".to_string()), "Renew");
    }
}
