//! # api::novel::wordpress
//!
//! Adapter for WordPress-based translation sites (e.g. `divinedaolibrary.com`).
//!
//! ## Supported themes
//! - **Fictioneer** (used by DivineDaoLibrary since their redesign): story
//!   pages under `/story/<slug>/`, chapters listed as
//!   `a.chapter-group__list-item-link`, content in `#chapter-content`.
//! - **Classic WordPress**: chapter links inside the post body
//!   (`div.entry-content`), content is the post body itself.
//!
//! When the pasted URL is a *chapter* page (no chapter list on it), the
//! adapter climbs one path segment up and retries — so pasting any chapter
//! link of a Fictioneer story finds the story overview automatically.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and HTML utilities

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, host_of, looks_like_chapter_text, sanitize_to_xhtml,
    ChapterContent, ChapterRef, NovelInfo, NovelSource, PoliteClient,
};
use crate::error::{Result, VaultError};

/// Content selectors for chapter pages, in priority order.
/// Fictioneer (`#chapter-content`) first, then classic WordPress bodies.
const CHAPTER_CONTENT_SELECTORS: [&str; 5] = [
    "#chapter-content",
    ".chapter__content",
    "div.entry-content",
    "article .post-content",
    "article",
];

/// WordPress widget/cruft selectors removed from chapter content.
const CRUFT_SELECTORS: [&str; 4] = [
    ".sharedaddy",
    ".jp-relatedposts",
    "nav.post-navigation",
    ".wp-block-buttons",
];

/// WordPress translation-site adapter.
pub struct WordPressSource;

impl NovelSource for WordPressSource {
    fn id(&self) -> &'static str {
        "wordpress"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url)?;
        match parse_novel_info(&final_url, &body) {
            Ok(info) => Ok(info),
            Err(first_error) => {
                // The pasted URL may be a chapter page; climb one path segment
                // up (chapter → story overview) and retry once.
                let Some(parent) = parent_url(&final_url) else {
                    return Err(first_error);
                };
                let (parent_final, parent_body) = client.get_text(&parent)?;
                parse_novel_info(&parent_final, &parent_body).map_err(|_| first_error)
            }
        }
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let content = extract_chapter_content(&body).ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "WordPress-Kapitelinhalt nicht gefunden: {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: content,
        })
    }
}

/// Extracts and sanitizes the chapter body from a WordPress post page.
fn extract_chapter_content(body: &str) -> Option<String> {
    let html = Html::parse_document(body);
    let fragment = extract_content(&html, &CHAPTER_CONTENT_SELECTORS)?;
    Some(sanitize_to_xhtml(&strip_cruft(&fragment)))
}

/// Removes known WordPress widget markup from a fragment before sanitizing.
///
/// Re-parses the fragment and re-serializes everything that does not match a
/// cruft selector.  This runs before the whitelist sanitizer, which would
/// otherwise unwrap widget containers and keep their text.
fn strip_cruft(fragment: &str) -> String {
    let parsed = Html::parse_fragment(fragment);
    let cruft: Vec<Selector> = CRUFT_SELECTORS
        .iter()
        .filter_map(|raw| Selector::parse(raw).ok())
        .collect();

    let cruft_ids: std::collections::HashSet<_> = cruft
        .iter()
        .flat_map(|selector| parsed.select(selector).map(|element| element.id()))
        .collect();

    let mut out = String::with_capacity(fragment.len());
    for child in parsed.tree.root().children() {
        serialize_without(child, &cruft_ids, &mut out);
    }
    out
}

fn serialize_without(
    node: ego_tree::NodeRef<'_, scraper::Node>,
    skip: &std::collections::HashSet<ego_tree::NodeId>,
    out: &mut String,
) {
    if skip.contains(&node.id()) {
        return;
    }
    match node.value() {
        scraper::Node::Text(text) => out.push_str(&crate::core::epub::escape_xml(&text.text)),
        scraper::Node::Element(element) => {
            let name = element.name();
            out.push_str(&format!("<{name}>"));
            for child in node.children() {
                serialize_without(child, skip, out);
            }
            out.push_str(&format!("</{name}>"));
        }
        _ => {
            for child in node.children() {
                serialize_without(child, skip, out);
            }
        }
    }
}

/// Strips the last path segment ("…/story/x/chapter-1/" → "…/story/x/").
fn parent_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = without_query.trim_end_matches('/');
    let scheme_end = trimmed.find("://").map(|i| i + 3)?;
    let last_slash = trimmed.rfind('/')?;
    // Refuse to climb above the host root.
    if last_slash <= scheme_end {
        return None;
    }
    Some(format!("{}/", &trimmed[..last_slash]))
}

/// Parses a novel overview page: Fictioneer structure first, then classic
/// WordPress post bodies.
fn parse_novel_info(page_url: &str, body: &str) -> Result<NovelInfo> {
    let html = Html::parse_document(body);
    if let Some(info) = parse_fictioneer_story(page_url, &html) {
        return Ok(info);
    }
    parse_classic_wordpress(page_url, &html)
}

/// Parses a Fictioneer-theme story page (DivineDaoLibrary and friends).
fn parse_fictioneer_story(page_url: &str, html: &Html) -> Option<NovelInfo> {
    let chapter_selector = Selector::parse("a.chapter-group__list-item-link").ok()?;
    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&chapter_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let text = link.text().collect::<Vec<_>>().join(" ");
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
        return None;
    }

    let title = first_text(html, "h1.story__identity-title")?;
    let cover_url = super::og_image(html)
        .or_else(|| first_attr(html, "img.story__thumbnail-image", "src"))
        .map(|src| absolutize(page_url, &src));
    let description = first_text(html, ".story__summary");

    Some(NovelInfo {
        title,
        author: first_text(html, ".story__author a").or_else(|| first_text(html, ".story__author")),
        cover_url,
        description,
        completed_hint: None,
        genres: Vec::new(),
        tags: Vec::new(),
        chapters,
    })
}

/// Parses a classic WordPress novel page (chapter links in the post body).
fn parse_classic_wordpress(page_url: &str, html: &Html) -> Result<NovelInfo> {
    let page_host = host_of(page_url).unwrap_or_default();

    let title = first_text(html, "h1.entry-title")
        .or_else(|| first_text(html, "h1"))
        .ok_or_else(|| {
            VaultError::ExternalApi(format!("Novel-Titel nicht gefunden: {page_url}"))
        })?;

    let link_selector = Selector::parse("div.entry-content a[href]")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;

    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&link_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let text = link.text().collect::<Vec<_>>().join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !looks_like_chapter_text(&text) {
            continue;
        }
        let url = absolutize(page_url, href);
        // Only same-host links are chapters; external links are ads or socials.
        if host_of(&url).unwrap_or_default() != page_host {
            continue;
        }
        if seen.insert(url.clone()) {
            chapters.push(ChapterRef { title: text, url });
        }
    }

    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Keine Kapitel-Links auf der Seite gefunden: {page_url}"
        )));
    }

    Ok(NovelInfo {
        title,
        author: None,
        cover_url: super::og_image(html).map(|src| absolutize(page_url, &src)),
        description: None,
        completed_hint: None,
        genres: Vec::new(),
        tags: Vec::new(),
        chapters,
    })
}

/// First matching element's attribute value.
fn first_attr(html: &Html, raw_selector: &str, attr: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr(attr)
        .map(str::to_string)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const NOVEL_PAGE: &str = r#"
    <html><body>
      <h1 class="entry-title">Martial Test God</h1>
      <div class="entry-content">
        <p>Synopsis text here.</p>
        <ul>
          <li><a href="https://www.example-wp.com/mtg-chapter-1/">Chapter 1 – Awakening</a></li>
          <li><a href="https://www.example-wp.com/mtg-chapter-2/">Chapter 2 – Rise</a></li>
        </ul>
        <p><a href="https://twitter.com/share">Share on Twitter</a></p>
        <p><a href="https://www.example-wp.com/about/">About the Author</a></p>
      </div>
    </body></html>"#;

    #[test]
    fn parses_chapter_links_same_host_only() {
        let info = parse_novel_info("https://www.example-wp.com/mtg/", NOVEL_PAGE)
            .expect("page should parse");
        assert_eq!(info.title, "Martial Test God");
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Chapter 1 – Awakening");
        assert_eq!(
            info.chapters[1].url,
            "https://www.example-wp.com/mtg-chapter-2/"
        );
    }

    const FICTIONEER_PAGE: &str = r#"
    <html><head>
      <meta property="og:image" content="https://www.example-ddl.com/wp-content/uploads/cover-200x300.jpg"/>
    </head><body>
      <h1 class="story__identity-title">Rebuild World</h1>
      <section class="story__summary"><p>Akira starts from the bottom.</p></section>
      <ol class="chapter-group__list">
        <li class="chapter-group__list-item">
          <a href='https://www.example-ddl.com/story/rebuild-world/chapter-1/'
             class="chapter-group__list-item-link truncate _1-1 ">Chapter 1, Akira and Alpha</a>
        </li>
        <li class="chapter-group__list-item">
          <a href='https://www.example-ddl.com/story/rebuild-world/chapter-2/'
             class="chapter-group__list-item-link truncate _1-1 ">Chapter 2, Getting Paid</a>
        </li>
      </ol>
    </body></html>"#;

    #[test]
    fn parses_fictioneer_story_page() {
        let info = parse_novel_info(
            "https://www.example-ddl.com/story/rebuild-world/",
            FICTIONEER_PAGE,
        )
        .expect("fictioneer page should parse");
        assert_eq!(info.title, "Rebuild World");
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Chapter 1, Akira and Alpha");
        assert_eq!(
            info.chapters[1].url,
            "https://www.example-ddl.com/story/rebuild-world/chapter-2/"
        );
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://www.example-ddl.com/wp-content/uploads/cover-200x300.jpg")
        );
        assert!(info
            .description
            .as_deref()
            .unwrap_or("")
            .contains("Akira starts"));
    }

    #[test]
    fn parent_url_climbs_one_segment() {
        assert_eq!(
            parent_url("https://www.example-ddl.com/story/rebuild-world/chapter-1/").as_deref(),
            Some("https://www.example-ddl.com/story/rebuild-world/")
        );
        assert_eq!(
            parent_url("https://www.example-ddl.com/story/").as_deref(),
            Some("https://www.example-ddl.com/")
        );
        assert_eq!(parent_url("https://www.example-ddl.com/"), None);
        assert_eq!(parent_url("https://www.example-ddl.com"), None);
    }

    #[test]
    fn fictioneer_chapter_content_extracts() {
        let page = r#"<html><body><article class="chapter__article">
            <section id="chapter-content" class="chapter__content content-section">
              <p>Chapter text here.</p>
            </section></article></body></html>"#;
        let content = extract_chapter_content(page).expect("content should extract");
        assert!(content.contains("Chapter text here."));
    }

    #[test]
    fn strips_wordpress_cruft() {
        let content = r#"<div class="entry-content">
            <p>Story text.</p>
            <div class="sharedaddy"><p>Share this!</p></div>
            <p>More story.</p></div>"#;
        let html = format!("<html><body><article>{content}</article></body></html>");
        let extracted = extract_chapter_content(&html).expect("content should extract");
        assert!(extracted.contains("Story text."));
        assert!(extracted.contains("More story."));
        assert!(!extracted.contains("Share this!"));
    }
}
