//! # api::manga::webtoons
//!
//! Webtoons adapter (`webtoons.com`).
//!
//! ## Page model
//! Webtoons are vertical strips: one episode is a long column of image slices
//! rather than discrete pages.  The viewer lists them in `#_imageList` with the
//! real source in `data-url` (the `src` attribute holds a lazy-load
//! placeholder).  Slices are packaged into the CBZ in document order, so the
//! chapter reads top to bottom.
//!
//! ## Referer requirement
//! The image CDN answers `403` unless the request carries a `webtoons.com`
//! referer, so every [`PageImage`] produced here records the viewer URL.
//!
//! ## Episode list pagination
//! The list page shows a fixed window of episodes with `&page=N` for the rest.
//! Paging continues until a page yields no unseen episode, which also
//! terminates cleanly when a series is shorter than one page.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and URL helpers

use scraper::{Html, Selector};

use super::{tidy_text, MangaChapterRef, MangaInfo, MangaSource, PageImage};
use crate::api::novel::{absolutize, PoliteClient};
use crate::error::{FeroError, Result};

/// Referer sent with every page-image request.
const IMAGE_REFERER: &str = "https://www.webtoons.com/";

/// Upper bound on episode-list pages walked per series.
///
/// At ~10 episodes per page this covers a 2000-episode daily strip while
/// bounding the request count if pagination ever stops terminating.
const MAX_LIST_PAGES: u32 = 200;

/// Webtoons adapter.
pub struct WebtoonsSource;

impl MangaSource for WebtoonsSource {
    fn id(&self) -> &'static str {
        "webtoons"
    }

    fn fetch_series_info(&self, client: &PoliteClient, url: &str) -> Result<MangaInfo> {
        let (final_url, body) = client.get_text(url)?;
        let mut info = parse_list_page(&final_url, &body)?;

        // Page 1 is already parsed; keep requesting until nothing new appears.
        let mut seen: std::collections::HashSet<String> =
            info.chapters.iter().map(|c| c.url.clone()).collect();
        for page in 2..=MAX_LIST_PAGES {
            let page_url = list_page_url(&final_url, page);
            let (page_final_url, page_body) = client.get_text(&page_url)?;
            let page_info = parse_list_page(&page_final_url, &page_body)?;

            let fresh: Vec<MangaChapterRef> = page_info
                .chapters
                .into_iter()
                .filter(|chapter| seen.insert(chapter.url.clone()))
                .collect();
            if fresh.is_empty() {
                break;
            }
            info.chapters.extend(fresh);
        }

        // Episode numbers are authoritative for order; the list itself is
        // newest-first and pages may overlap.
        info.chapters
            .sort_by_key(|chapter| episode_number(&chapter.url).unwrap_or(0));
        Ok(info)
    }

    fn fetch_chapter_pages(
        &self,
        client: &PoliteClient,
        chapter: &MangaChapterRef,
    ) -> Result<Vec<PageImage>> {
        let (final_url, body) = client.get_text(&chapter.url)?;
        let urls = parse_viewer_images(&final_url, &body)?;
        Ok(urls
            .into_iter()
            .map(|url| PageImage::with_referer(url, IMAGE_REFERER))
            .collect())
    }
}

/// Builds the URL of episode-list page `page`.
fn list_page_url(list_url: &str, page: u32) -> String {
    let base = match list_url.split_once("&page=") {
        Some((before, _)) => before,
        None => list_url,
    };
    format!("{base}&page={page}")
}

/// Extracts the `episode_no` query parameter of a viewer URL.
fn episode_number(url: &str) -> Option<u32> {
    let after = url.split("episode_no=").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Parses one episode-list page: series metadata plus its episode links.
fn parse_list_page(page_url: &str, body: &str) -> Result<MangaInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, ".detail_header .subj")
        .or_else(|| first_text(&html, "h1.subj"))
        .or_else(|| first_text(&html, ".subj"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("Webtoons-Titel nicht gefunden: {page_url}"))
        })?;

    let link_selector = Selector::parse("a[href*='viewer?title_no=']")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&link_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let url = absolutize(page_url, href);
        if !seen.insert(url.clone()) {
            continue;
        }
        // `.subj` wraps the title in an inner span and appends an `<em>` badge
        // ("UP", "NEW") for recent episodes; taking the inner span keeps the
        // badge out of the title — and out of the CBZ file name.
        let label = first_child_text(&link, ".subj span")
            .or_else(|| first_child_text(&link, ".subj"))
            .unwrap_or_else(|| tidy_text(&link.text().collect::<String>()));
        let number = episode_number(&url).map(|number| number.to_string());
        chapters.push(MangaChapterRef {
            title: if label.is_empty() {
                match &number {
                    Some(number) => format!("Episode {number}"),
                    None => url.clone(),
                }
            } else {
                label
            },
            url,
            volume: None,
            number,
        });
    }

    Ok(MangaInfo {
        title,
        author: own_text(&html, ".detail_header .author")
            .or_else(|| own_text(&html, ".author_area")),
        artist: None,
        cover_url: first_attr(&html, ".detail_body .thmb img", "src")
            .or_else(|| crate::api::novel::og_image(&html))
            .map(|src| absolutize(page_url, &src)),
        description: first_text(&html, ".detail_body .summary"),
        completed_hint: first_text(&html, ".detail_header .txt_ico_completed").map(|_| true),
        genres: collect_texts(&html, ".detail_header .genre"),
        tags: Vec::new(),
        // Webtoons are read top-to-bottom, left-to-right.
        right_to_left: false,
        chapters,
    })
}

/// Reads the ordered strip images from a viewer page.
fn parse_viewer_images(page_url: &str, body: &str) -> Result<Vec<String>> {
    let html = Html::parse_document(body);
    let selector = Selector::parse("#_imageList img")
        .map_err(|e| FeroError::ExternalApi(format!("selector parse error: {e}")))?;

    let images: Vec<String> = html
        .select(&selector)
        .filter_map(|img| {
            // `src` is a lazy-load placeholder; the real URL is in `data-url`.
            let src = img
                .value()
                .attr("data-url")
                .or_else(|| img.value().attr("src"))?;
            Some(absolutize(page_url, src))
        })
        .collect();

    if images.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Keine Bilder im Webtoons-Viewer gefunden \
             (Episode kostenpflichtig oder nicht verfügbar?): {page_url}"
        )));
    }
    Ok(images)
}

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

fn first_child_text(element: &scraper::ElementRef, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let text = tidy_text(&element.select(&selector).next()?.text().collect::<String>());
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

/// Text of an element's own child text nodes, ignoring nested elements.
///
/// The author block ends with an "author info" link; descending into it would
/// glue that UI label onto the author name.
fn own_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let own: String = element
        .children()
        .filter_map(|child| child.value().as_text().map(|text| text.to_string()))
        .collect();
    let text = tidy_text(&own);
    // Some layouts wrap the name in a link; fall back to the full subtree
    // rather than reporting no author at all.
    if text.is_empty() {
        first_text(html, raw_selector)
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

    const LIST_PAGE: &str = r#"
    <html><body>
      <div class="detail_header type_black">
        <p class="subj">Jungle Juice</p>
        <div class="author">Hyeong Eun</div>
        <h2 class="genre">Action</h2>
      </div>
      <div class="detail_body">
        <div class="summary">Ein Insektenmensch.</div>
      </div>
      <ul>
        <li><a href="https://www.webtoons.com/en/action/jungle-juice/ep-2/viewer?title_no=2480&amp;episode_no=2">
          <span class="subj"><span>Episode 2</span></span></a></li>
        <li><a href="https://www.webtoons.com/en/action/jungle-juice/ep-1/viewer?title_no=2480&amp;episode_no=1">
          <span class="subj"><span>Episode 1</span></span></a></li>
      </ul>
    </body></html>"#;

    const VIEWER_PAGE: &str = r#"
    <html><body>
      <div id="_imageList">
        <img src="//placeholder.example/blank.gif" data-url="//webtoon-phinf.pstatic.net/a/1.jpg">
        <img src="//placeholder.example/blank.gif" data-url="//webtoon-phinf.pstatic.net/a/2.jpg">
      </div>
    </body></html>"#;

    #[test]
    fn parses_series_metadata_and_episode_links() {
        let info = parse_list_page(
            "https://www.webtoons.com/en/action/jungle-juice/list?title_no=2480",
            LIST_PAGE,
        )
        .expect("list page should parse");

        assert_eq!(info.title, "Jungle Juice");
        assert_eq!(info.author.as_deref(), Some("Hyeong Eun"));
        assert_eq!(info.genres, vec!["Action".to_string()]);
        assert!(!info.right_to_left);
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].number.as_deref(), Some("2"));
    }

    #[test]
    fn strip_images_come_from_data_url_not_the_placeholder() {
        let images = parse_viewer_images(
            "https://www.webtoons.com/en/action/jungle-juice/ep-1/viewer?title_no=2480&episode_no=1",
            VIEWER_PAGE,
        )
        .expect("viewer should parse");

        assert_eq!(images.len(), 2);
        assert_eq!(images[0], "https://webtoon-phinf.pstatic.net/a/1.jpg");
        assert!(!images.iter().any(|url| url.contains("placeholder")));
    }

    #[test]
    fn paywalled_episode_reports_a_clear_reason() {
        let error = parse_viewer_images(
            "https://www.webtoons.com/en/action/x/ep/viewer?title_no=1&episode_no=1",
            "<html><body><div id=\"_imageList\"></div></body></html>",
        )
        .expect_err("empty viewer should error");
        assert!(error.to_string().contains("kostenpflichtig"));
    }

    #[test]
    fn builds_pagination_urls_without_stacking_page_params() {
        let base = "https://www.webtoons.com/en/action/x/list?title_no=1";
        assert_eq!(list_page_url(base, 2), format!("{base}&page=2"));
        // A URL that already carries `&page=` must be rewritten, not appended.
        assert_eq!(
            list_page_url(&format!("{base}&page=2"), 3),
            format!("{base}&page=3")
        );
    }

    #[test]
    fn reads_episode_numbers_for_ordering() {
        assert_eq!(
            episode_number("https://www.webtoons.com/en/a/b/viewer?title_no=1&episode_no=42"),
            Some(42)
        );
        assert_eq!(episode_number("https://www.webtoons.com/en/a/b/list"), None);
    }
}
