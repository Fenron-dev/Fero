//! # api::novel::novelphoenix
//!
//! Adapter for the LightNovelWorld-style engine used by `novelphoenix.com`.
//!
//! ## Page structure (verified 09/2026)
//! - Novel page `/novel/<slug>`: title `h1.novel-title`, author `.author a`,
//!   cover `og:image`, synopsis `.summary .content`, genres in the header's
//!   `.categories` list, tags in `.tags` (only on novels that carry any) and
//!   the status as the `<strong>` labelled by `<small>Status</small>` inside
//!   `.header-stats`.
//! - Chapter list `/novel/<slug>/chapters?page=N` (100 per page, `?page=`
//!   pagination): each row `ul.chapter-list li a` holds the clean title in
//!   `strong.chapter-title` and an exact release date in
//!   `time.chapter-update[datetime]`; the list header repeats the newest one
//!   and names the status again as `strong.status`.
//! - Chapter page: content in `#chapter-container`.
//!
//! Three traps this engine sets, each of which cost metadata before:
//! - Genre links read `/genre-action/sort-new/status-all/all-novel`, not
//!   `/genre/<name>` — and the page footer advertises the same shape. Matching
//!   on the href would append the footer's genres to every novel, so the list
//!   is read from its position in the header instead.
//! - The status is not in a `.status` element on the novel page; it is an
//!   unnamed `<strong>` whose sibling `<small>` carries the label. The visible
//!   label is the stable half, so that is what identifies it.
//! - A whole chapter row sits inside one `<a>`, so its text reads
//!   "1 Chapter 1 : Send Me to the Past 1 year ago". The title has to come from
//!   the inner `strong`, otherwise the chapter number and the age end up in the
//!   EPUB's table of contents.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client and HTML utilities
//! - `api::release_date` – release dates into timestamps

use scraper::{Html, Selector};

use super::{
    absolutize, extract_content, og_image, sanitize_to_xhtml, ChapterContent, ChapterRef,
    NovelInfo, NovelSource, PoliteClient,
};
use crate::api::release_date;
use crate::core::subscription::unix_now;
use crate::error::{FeroError, Result};

/// Content selectors for chapter pages, in priority order.
const CHAPTER_CONTENT_SELECTORS: [&str; 3] = ["#chapter-container", ".chapter-content", "#content"];

/// Hard cap on ToC pages so broken pagination can never loop forever.
const MAX_TOC_PAGES: u32 = 200;

/// Where the status may sit once the header stats block cannot be read.
///
/// `strong.status` is what the chapter-list page uses; the class-only variants
/// are the novel page's own markup, which names the state as a CSS class.
const STATUS_FALLBACK_SELECTORS: [&str; 4] = [
    "strong.status",
    ".header-stats strong.completed",
    ".header-stats strong.ongoing",
    ".status",
];

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
        let now = unix_now();

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
            // Rows carry their own dates, so the newest one may be on any page
            // — the list is oldest first, but a repaired chapter can be dated
            // later than the one after it.
            info.latest_release_unix =
                newer_of(info.latest_release_unix, newest_release(&page_html, now));
            // The list page repeats the status in markup of its own. Reading it
            // as a fallback means a redesign of one page still leaves the other.
            if info.completed_hint.is_none() {
                let fallback = status_text(&page_html);
                info.completed_hint = fallback.as_deref().and_then(completed_from_status);
                info.source_status_text = info.source_status_text.take().or(fallback);
            }
            if page >= last_pagination_page(&page_html).min(MAX_TOC_PAGES) {
                break;
            }
            page += 1;
        }

        let mut seen = std::collections::HashSet::new();
        info.chapters
            .retain(|chapter| seen.insert(chapter.url.clone()));

        if info.chapters.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Kapitel auf der Seite gefunden: {final_url}"
            )));
        }
        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (_final_url, body) = client.get_text(&chapter.url)?;
        let html = Html::parse_document(&body);
        let content = extract_content(&html, &CHAPTER_CONTENT_SELECTORS).ok_or_else(|| {
            FeroError::ExternalApi(format!("Kapitelinhalt nicht gefunden: {}", chapter.url))
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
        .ok_or_else(|| FeroError::ExternalApi(format!("Novel-Titel nicht gefunden: {page_url}")))?;
    let status = status_text(html);

    Ok(NovelInfo {
        title,
        author: first_text(html, ".author a").or_else(|| first_text(html, "[itemprop='author']")),
        cover_url: og_image(html).map(|src| absolutize(page_url, &src)),
        description: summary_text(html),
        completed_hint: status.as_deref().and_then(completed_from_status),
        source_status_text: status,
        // Filled while walking the chapter list, where the dates are.
        latest_release_unix: None,
        genres: collect_link_texts(html, ".categories ul li a"),
        tags: collect_link_texts(html, ".tags a.tag"),
        chapters: Vec::new(),
    })
}

/// Synopsis without the "Show More" control the engine puts in the same box.
///
/// Taking `.summary` as a whole would contribute its "Summary" heading and the
/// button's label to the description — and a description is written into the
/// EPUB, where nobody can edit it out afterwards.
fn summary_text(html: &Html) -> Option<String> {
    let container = Selector::parse(".summary .content").ok()?;
    let content = html.select(&container).next()?;
    let button = Selector::parse(".expand").ok()?;
    let expand: std::collections::HashSet<_> =
        content.select(&button).map(|node| node.id()).collect();

    let mut raw = String::new();
    for child in content.children() {
        push_text_without(child, &expand, &mut raw);
    }
    let text = tidy(&raw);
    (!text.is_empty()).then_some(text)
}

/// Collects text, leaving out the subtrees named in `skip`.
fn push_text_without(
    node: ego_tree::NodeRef<'_, scraper::Node>,
    skip: &std::collections::HashSet<ego_tree::NodeId>,
    out: &mut String,
) {
    if skip.contains(&node.id()) {
        return;
    }
    if let scraper::Node::Text(text) = node.value() {
        out.push_str(&text.text);
        return;
    }
    // A paragraph or a line break separates words that would otherwise run
    // into each other once the whitespace is collapsed.
    if matches!(
        node.value().as_element().map(|element| element.name()),
        Some("p" | "br")
    ) {
        out.push(' ');
    }
    for child in node.children() {
        push_text_without(child, skip, out);
    }
}

/// The status as the page prints it, or `None` when no shape matches.
fn status_text(html: &Html) -> Option<String> {
    if let (Ok(entries), Ok(label), Ok(value)) = (
        Selector::parse(".header-stats span"),
        Selector::parse("small"),
        Selector::parse("strong"),
    ) {
        for entry in html.select(&entries) {
            let labelled_status = entry
                .select(&label)
                .next()
                .map(|node| tidy(&node.text().collect::<String>()).to_lowercase())
                .is_some_and(|text| text.starts_with("status"));
            if !labelled_status {
                continue;
            }
            if let Some(text) = entry
                .select(&value)
                .next()
                .map(|node| tidy(&node.text().collect::<String>()))
                .filter(|text| !text.is_empty())
            {
                return Some(text);
            }
        }
    }

    STATUS_FALLBACK_SELECTORS
        .iter()
        .find_map(|selector| first_text(html, selector))
}

/// Maps the source's own wording onto Fero's finished/unfinished question.
///
/// "Hiatus" answers neither — a paused translation is not a finished one, and
/// claiming it is unfinished would say more than the page does.
fn completed_from_status(text: &str) -> Option<bool> {
    let lower = text.to_lowercase();
    if lower.contains("completed") || lower.contains("finished") {
        Some(true)
    } else if lower.contains("ongoing") || lower.contains("on-going") {
        Some(false)
    } else {
        None
    }
}

/// Newest release date on a chapter-list page.
///
/// Every row carries a `<time datetime="…">` and the page header repeats the
/// newest one in the same shape, so the maximum over all of them is the answer
/// whichever of the two the engine keeps. The visible text ("2 days ago") is
/// the fallback for rows that print no attribute.
fn newest_release(html: &Html, now: u64) -> Option<u64> {
    let selector = Selector::parse("time").ok()?;
    let mut newest: Option<u64> = None;
    for node in html.select(&selector) {
        let released = node
            .value()
            .attr("datetime")
            .and_then(|value| release_date::parse_release(value, now))
            .or_else(|| release_date::parse_release(&node.text().collect::<String>(), now));
        newest = newer_of(newest, released);
    }
    newest
}

/// The later of two optional timestamps.
fn newer_of(known: Option<u64>, found: Option<u64>) -> Option<u64> {
    match (known, found) {
        (Some(known), Some(found)) => Some(known.max(found)),
        (known, found) => known.or(found),
    }
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
    let inner_title = Selector::parse("strong.chapter-title").ok();
    let mut chapters = Vec::new();
    for link in html.select(selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.contains("/chapter-") {
            continue;
        }
        let title = inner_title
            .as_ref()
            .and_then(|selector| link.select(selector).next())
            .map(|node| tidy(&node.text().collect::<String>()))
            .or_else(|| link.value().attr("title").map(tidy))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| tidy(&link.text().collect::<String>()));
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

/// Collapses all whitespace runs into single spaces.
fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let element = html.select(&selector).next()?;
    let text = tidy(&element.text().collect::<String>());
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
        .map(|link| tidy(&link.text().collect::<String>()))
        .filter(|text| !text.is_empty() && seen.insert(text.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed copy of a real chapter-list page (09/2026): the row is one link
    /// around number, title and age, and the header repeats the newest date.
    const CHAPTERS_PAGE: &str = r#"
    <html><body><article id="chapter-list-page">
      <header class="container"><div class="novel-item"><div class="item-body">
        <div class="novel-stats"><span>Updated <time datetime="2026-09-24 11:25">2 days ago</time></span></div>
        <div class="novel-stats">Status: <strong class="status">Ongoing</strong></div>
      </div></div></header>
      <ul class="chapter-list">
        <li><a href="/novel/magus-infinite/chapter-1" title="Chapter 1 : Send Me to the Past"><span class="chapter-no">1</span><strong class="chapter-title">Chapter 1 : Send Me to the Past</strong><time class="chapter-update" datetime="2024-11-02 06:30:01">1 year ago</time></a></li>
        <li><a href="/novel/magus-infinite/chapter-2" title="Chapter 2 : Did You Push Me Away"><span class="chapter-no">2</span><strong class="chapter-title">Chapter 2 : Did You Push Me Away</strong><time class="chapter-update" datetime="2024-11-02 06:31:01">1 year ago</time></a></li>
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
    fn chapter_titles_leave_out_the_row_number_and_the_age() {
        let html = Html::parse_document(CHAPTERS_PAGE);
        let chapters = parse_chapter_links("https://novelphoenix.com/novel/magus-infinite", &html);
        assert_eq!(chapters[0].title, "Chapter 1 : Send Me to the Past");
        assert_eq!(chapters[1].title, "Chapter 2 : Did You Push Me Away");
    }

    #[test]
    fn newest_release_comes_from_the_exact_attribute() {
        let html = Html::parse_document(CHAPTERS_PAGE);
        // 2026-09-24 in the header beats the 2024 rows.
        assert_eq!(newest_release(&html, 1_790_000_000), Some(1_790_208_000));
    }

    #[test]
    fn relative_row_dates_still_answer_without_an_attribute() {
        let html = Html::parse_document(
            r#"<ul class="chapter-list"><li><a href="/novel/x/chapter-9">
               <strong class="chapter-title">Chapter 9</strong>
               <time class="chapter-update">2 days ago</time></a></li></ul>"#,
        );
        let now = 1_790_000_000;
        assert_eq!(newest_release(&html, now), Some(now - 2 * 86_400));
    }

    #[test]
    fn chapter_list_page_answers_the_status_too() {
        let html = Html::parse_document(CHAPTERS_PAGE);
        assert_eq!(
            status_text(&html)
                .as_deref()
                .and_then(completed_from_status),
            Some(false)
        );
    }

    #[test]
    fn builds_chapters_url() {
        assert_eq!(
            chapters_base_url("https://novelphoenix.com/novel/magus-infinite/"),
            "https://novelphoenix.com/novel/magus-infinite/chapters"
        );
    }

    /// Trimmed copy of a real novel page (09/2026). Note the genre href shape,
    /// the status without a `.status` element, and the footer that repeats the
    /// same genre links.
    const NOVEL_PAGE: &str = r#"
    <html><head><meta property="og:image" content="/server-1/magus.jpg"/></head><body>
      <article id="novel"><header class="novel-header"><div class="header-body container">
        <div class="novel-info">
          <div class="main-head">
            <h1 class="novel-title text2row">MAGUS INFINITE</h1>
            <div class="author"><span>Author:</span> <a href="/author/x"><span itemprop="author">BRICKTRADER</span></a></div>
          </div>
          <div class="header-stats">
            <span><strong>854</strong><small>Chapters</small></span>
            <span><strong>481.4K</strong><small>Views</small></span>
            <span> <strong class="ongoing">Ongoing</strong> <small>Status</small></span>
          </div>
          <div class="categories"><h4>Genres</h4><ul>
            <li><a href="/genre-fantasy/sort-new/status-all/all-novel" class="property-item">Fantasy</a></li>
            <li><a href="/genre-action/sort-new/status-all/all-novel" class="property-item">Action</a></li>
          </ul></div>
          <div class="categories sharethis-inline-share-buttons my-md-2"></div>
        </div>
      </div></header>
      <section id="info">
        <div class="summary"><h4 class="lined">Summary</h4>
          <div class="content expand-wrapper"><p>A mage climbs forever.</p><p>Associated Names: 절대회귀</p>
            <div class="expand"><a class="expand-btn"><span>Show More</span></a></div>
          </div>
        </div>
        <div class="tags mt-lg-1 mt-3 clearfix"><h4 class="lined">Tags</h4><div class="expand-wrapper">
          <ul class="content"><li><a class="tag" href="/tags/gods/order-popular" rel="tag">Gods</a></li>
          <li><a class="tag" href="/tags/monsters/order-popular" rel="tag">Monsters</a></li></ul>
        </div></div>
      </section></article>
      <footer><nav class="col links"><ul>
        <li><a href="/genre-romance/sort-popular/status-all/all-novel">Romance</a></li>
        <li><a href="/genre-josei/sort-popular/status-all/all-novel">Josei</a></li>
      </ul></nav></footer>
    </body></html>"#;

    #[test]
    fn parses_novel_metadata() {
        let html = Html::parse_document(NOVEL_PAGE);
        let info = parse_novel_page("https://novelphoenix.com/novel/magus-infinite", &html)
            .expect("should parse");
        assert_eq!(info.title, "MAGUS INFINITE");
        assert_eq!(info.author.as_deref(), Some("BRICKTRADER"));
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(info.source_status_text.as_deref(), Some("Ongoing"));
        assert_eq!(
            info.cover_url.as_deref(),
            Some("https://novelphoenix.com/server-1/magus.jpg")
        );
    }

    #[test]
    fn genres_come_from_the_header_not_from_the_footer() {
        let html = Html::parse_document(NOVEL_PAGE);
        let info = parse_novel_page("https://novelphoenix.com/novel/magus-infinite", &html)
            .expect("should parse");
        assert_eq!(
            info.genres,
            vec!["Fantasy".to_string(), "Action".to_string()]
        );
        assert_eq!(info.tags, vec!["Gods".to_string(), "Monsters".to_string()]);
    }

    #[test]
    fn description_leaves_out_heading_and_show_more_button() {
        let html = Html::parse_document(NOVEL_PAGE);
        let description = summary_text(&html).expect("summary should parse");
        assert!(description.starts_with("A mage climbs forever."));
        assert!(description.contains("Associated Names"));
        assert!(!description.contains("Show More"));
        assert!(!description.contains("Summary"));
    }

    #[test]
    fn completed_novels_report_their_status() {
        let html = Html::parse_document(
            r#"<div class="header-stats"><span><strong class="completed">Completed</strong>
               <small>Status</small></span></div>"#,
        );
        assert_eq!(
            status_text(&html)
                .as_deref()
                .and_then(completed_from_status),
            Some(true)
        );
    }

    #[test]
    fn a_paused_translation_is_neither_finished_nor_running() {
        assert_eq!(completed_from_status("Hiatus"), None);
    }
}
