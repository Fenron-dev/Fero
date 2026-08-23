//! # api::novel::novelarrow
//!
//! NovelArrow adapter (`novelarrow.com`) — a Next.js SPA whose chapter list
//! and chapter text are rendered client-side. Fetched exclusively through the
//! embedded browser window (see `api::novel::WEBVIEW_ROUTED_HOSTS`).
//!
//! ## Page structure (verified 07/2026)
//! - Novel page `/novel/<slug>`: SSR carries `og:novel:*` meta tags and the
//!   `chapter_id`/`chapter_name` RSC arrays (~30 newest) — used as a windowless
//!   fallback chapter list.
//! - Chapters tab `/novel/<slug>?tab=chapters`: the browser renders the full
//!   chapter list here. Chapter URLs come in two shapes:
//!   `/chapter/<slug>/chapter-<N>-<title>` and the bare `/chapter/<slug>/<N>`.
//! - Chapter page: the body ships in the server-rendered Flight (RSC) payload
//!   (`chapterInfo.chapter_content` → `$<id>` text chunk), fetched directly.
//!
//! ## Dependencies:
//! - `api::novel` – shared HTTP client (browser-routed) and HTML utilities

use scraper::{Html, Selector};

use super::{sanitize_to_xhtml, ChapterContent, ChapterRef, NovelInfo, NovelSource, PoliteClient};
use crate::error::{FeroError, Result};

/// NovelArrow source adapter (browser-window routed).
pub struct NovelArrowSource;

impl NovelSource for NovelArrowSource {
    fn id(&self) -> &'static str {
        "novelarrow"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let slug = novel_slug(url).ok_or_else(|| {
            FeroError::ExternalApi(format!("NovelArrow-URL ohne Novel-Slug: {url}"))
        })?;
        // The chapters tab renders the full list in a real browser.
        let chapters_url = format!("{}?tab=chapters", strip_query(url));
        let (_final_url, body) = client.get_text(&chapters_url)?;
        let html = Html::parse_document(&body);

        let mut chapters = parse_chapter_links(&html, &slug);
        if chapters.is_empty() {
            // Fallback: direct-fetch the novel page and read the chapter list
            // from its server-rendered RSC (~30 newest). This avoids a second
            // window render — navigating the same path with a different query
            // triggers a client-side SPA route that never reloads our injected
            // script, hanging until timeout.
            if let Ok(bytes) = client.get_bytes(url) {
                chapters = parse_rsc_chapters(&String::from_utf8_lossy(&bytes), &slug);
            }
        }
        if chapters.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Kapitel auf der NovelArrow-Seite gefunden: {url}"
            )));
        }

        let status = meta_content(&html, "og:novel:status").unwrap_or_default();
        let completed_hint = if status.to_lowercase().contains("completed") {
            Some(true)
        } else if status.to_lowercase().contains("ongoing") {
            Some(false)
        } else {
            None
        };
        let genres = meta_content(&html, "og:novel:genre")
            .map(|g| {
                g.split(',')
                    .map(|s| title_case(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Ok(NovelInfo {
            title: meta_content(&html, "og:novel:novel_name")
                .or_else(|| first_text(&html, "h1"))
                .unwrap_or_else(|| slug.replace('-', " ")),
            author: meta_content(&html, "og:novel:author"),
            cover_url: meta_content(&html, "og:image"),
            description: meta_content(&html, "og:description")
                .or_else(|| first_text(&html, ".description, .synopsis")),
            completed_hint,
            genres,
            tags: Vec::new(),
            chapters,
        })
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        // NovelArrow server-renders the chapter body into its Flight (RSC)
        // payload, so a plain fetch is timing-independent and authoritative —
        // no browser window needed (the window is closed during a download run
        // anyway, which made a fallback there fail). A chapter with no body in
        // the payload is genuinely unavailable (e.g. locked with no preview).
        let bytes = client.get_bytes(&chapter.url)?;
        let raw = String::from_utf8_lossy(&bytes);
        let html = extract_flight_chapter(&raw).ok_or_else(|| {
            FeroError::ExternalApi(format!(
                "Kapitelinhalt nicht im Seiten-Payload gefunden \
                 (evtl. gesperrtes Premium-Kapitel): {}",
                chapter.url
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: sanitize_to_xhtml(&html),
        })
    }
}

/// Extracts the chapter body HTML from NovelArrow's server-rendered Flight
/// (RSC) payload. Returns `None` if no paragraph-bearing chunk is found.
fn extract_flight_chapter(raw: &str) -> Option<String> {
    let flight = collect_flight(raw);
    // `chapterInfo.chapter_content` is either a `$<id>` reference to a Flight
    // text chunk (full chapters) or inline HTML (premium chapters ship their
    // available text — usually a real excerpt — directly in the field). Accept
    // short bodies (author-note fillers are a single paragraph).
    if let Some(value) = chapter_content_value(&flight) {
        if let Some(id) = value.strip_prefix('$') {
            if let Some(html) = flight_chunk(&flight, id) {
                if has_prose(&html) {
                    return Some(html);
                }
            }
        } else if has_prose(&value) {
            return Some(value);
        }
    }
    // Heuristic fallback: the chunk with the most `<p>` tags. This is a guess,
    // so keep the stricter guard to avoid grabbing a metadata blob.
    largest_paragraph_chunk(&flight).filter(|html| html.matches("<p").count() >= 2)
}

/// Whether a Flight chunk carries real chapter prose (a paragraph or a
/// meaningful run of visible text).
fn has_prose(html: &str) -> bool {
    html.contains("<p") || html.chars().filter(|c| !c.is_whitespace()).count() >= 20
}

/// Concatenates and JSON-unescapes every `self.__next_f.push([1,"…"])` string
/// literal, reconstructing the raw Flight stream.
fn collect_flight(raw: &str) -> String {
    const MARKER: &str = "self.__next_f.push([1,";
    let bytes = raw.as_bytes();
    let mut out = String::new();
    let mut search = 0;
    while let Some(rel) = raw[search..].find(MARKER) {
        let start = search + rel + MARKER.len();
        if bytes.get(start) != Some(&b'"') {
            search = start;
            continue;
        }
        // Scan to the matching closing quote, honoring backslash escapes.
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        if i >= bytes.len() {
            break;
        }
        // `start` and `i` are ASCII quotes → safe slice boundaries.
        if let Ok(decoded) = serde_json::from_str::<String>(&raw[start..=i]) {
            out.push_str(&decoded);
        }
        search = i + 1;
    }
    out
}

/// Reads the `chapterInfo.chapter_content` value, JSON-unescaping it. The field
/// is a JSON string inside the Flight stream, so inline HTML keeps its `\"`
/// escapes — reading it naively would truncate at the first attribute quote.
/// Returns either a `$<id>` chunk reference or inline HTML.
fn chapter_content_value(flight: &str) -> Option<String> {
    const KEY: &str = "\"chapter_content\":\"";
    // Position at the value's opening quote.
    let open = flight.find(KEY)? + KEY.len() - 1;
    let bytes = flight.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => break,
            _ => i += 1,
        }
    }
    if i >= bytes.len() {
        return None;
    }
    // `open` and `i` sit on ASCII quotes → valid slice boundaries.
    serde_json::from_str::<String>(&flight[open..=i]).ok()
}

/// Reads a `<id>:T<hexlen>,<payload>` Flight text chunk by its declared length.
fn flight_chunk(flight: &str, id: &str) -> Option<String> {
    let head = format!("{id}:T");
    let at = if flight.starts_with(&head) {
        0
    } else {
        flight.find(&format!("\n{head}"))? + 1
    };
    let after = &flight[at + head.len()..];
    let comma = after.find(',')?;
    let payload = &after[comma + 1..];
    match usize::from_str_radix(after[..comma].trim(), 16) {
        Ok(len) if len > 0 && len <= payload.len() => {
            // `len` counts UTF-8 bytes of valid content → char boundary.
            Some(String::from_utf8_lossy(&payload.as_bytes()[..len]).into_owned())
        }
        // Length unusable (encoding drift) → cut at the next chunk marker.
        _ => Some(cut_at_next_chunk(payload)),
    }
}

/// Truncates a Flight payload at the next `\n<digits>:` chunk boundary.
fn cut_at_next_chunk(payload: &str) -> String {
    let bytes = payload.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && bytes.get(j) == Some(&b':') {
                break;
            }
        }
        i += 1;
    }
    payload[..i].to_string()
}

/// Fallback: the `T`-chunk containing the most `<p>` tags (the chapter body).
fn largest_paragraph_chunk(flight: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut best_p = 1usize;
    for (idx, _) in flight.match_indices(":T") {
        let after = &flight[idx + 2..];
        let Some(comma) = after.find(',') else {
            continue;
        };
        if usize::from_str_radix(after[..comma].trim(), 16).is_err() {
            continue;
        }
        let payload = cut_at_next_chunk(&after[comma + 1..]);
        let paragraphs = payload.matches("<p").count();
        if paragraphs > best_p {
            best_p = paragraphs;
            best = Some(payload);
        }
    }
    best
}

/// Extracts the novel slug from a `/novel/<slug>` URL.
fn novel_slug(url: &str) -> Option<String> {
    let after = url.split("/novel/").nth(1)?;
    let slug = after.split(['/', '?', '#']).next()?;
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

fn strip_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Collects every chapter link for `slug`, deduped and sorted by chapter
/// number ascending (oldest first). NovelArrow uses two URL shapes:
/// `/chapter/<slug>/chapter-<N>-<title>` and the bare `/chapter/<slug>/<N>`.
fn parse_chapter_links(html: &Html, slug: &str) -> Vec<ChapterRef> {
    let Ok(selector) = Selector::parse("a[href*='/chapter/']") else {
        return Vec::new();
    };
    let prefix = format!("/chapter/{slug}/");
    let mut found: Vec<(u32, ChapterRef)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in html.select(&selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(number) = chapter_number_for(href, &prefix) else {
            continue;
        };
        let url = absolutize_novelarrow(href);
        if !seen.insert(number) {
            continue;
        }
        let text = link.text().collect::<Vec<_>>().join(" ");
        let title = clean_chapter_title(&text, number);
        found.push((number, ChapterRef { title, url }));
    }
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, chapter)| chapter).collect()
}

/// Fallback chapter list from the novel page's server-rendered RSC: the
/// positionally-aligned `chapter_id` + `chapter_name` arrays (~30 newest).
/// Windowless and reliable — used when the rendered list can't be parsed.
fn parse_rsc_chapters(raw: &str, slug: &str) -> Vec<ChapterRef> {
    let flight = collect_flight(raw);
    let ids = rsc_string_array(&flight, "chapter_id");
    let names = rsc_string_array(&flight, "chapter_name");
    let mut found: Vec<(u32, ChapterRef)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, id) in ids.iter().enumerate() {
        let Some(number) = chapter_number_from_id(id) else {
            continue;
        };
        if !seen.insert(number) {
            continue;
        }
        let title = names
            .get(i)
            .map(|name| clean_chapter_title(name, number))
            .unwrap_or_else(|| format!("Chapter {number}"));
        // `id` is either a bare number or a `chapter-<N>-…` slug — both form a
        // valid `/chapter/<slug>/<id>` URL.
        let url = format!("https://novelarrow.com/chapter/{slug}/{id}");
        found.push((number, ChapterRef { title, url }));
    }
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, chapter)| chapter).collect()
}

/// Reads consecutive `"<key>":"<value>"` string values out of a Flight stream.
fn rsc_string_array(flight: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\":\"");
    let bytes = flight.as_bytes();
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = flight[search..].find(&needle) {
        let start = search + rel + needle.len();
        // Values in the decoded stream carry no escapes → read to next quote.
        let mut i = start;
        while i < bytes.len() && bytes[i] != b'"' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        out.push(flight[start..i].to_string());
        search = i + 1;
    }
    out
}

/// Normalizes a NovelArrow chapter-link label. The anchors repeat a short
/// "C<N>: …" and a full "Chapter <N>: …" label; keep the canonical "Chapter …"
/// half to avoid duplicated EPUB titles like "C1: X Chapter 1: X".
fn clean_chapter_title(text: &str, number: u32) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return format!("Chapter {number}");
    }
    // Prefer the exact "Chapter <number>: …" label; this drops a leading short
    // label ("C1: …" or a bare "Chapter") that the anchor prepends.
    if let Some(pos) = collapsed.find(&format!("Chapter {number}")) {
        return collapsed[pos..].trim().to_string();
    }
    collapsed
}

/// Parses the chapter number from an href, given the `/chapter/<slug>/` prefix.
/// Accepts `chapter-<N>-<title>` and bare `<N>` chapter segments.
fn chapter_number_for(href: &str, prefix: &str) -> Option<u32> {
    let idx = href.find(prefix)?;
    let tail = &href[idx + prefix.len()..];
    let segment = tail.split(['/', '?', '#']).next()?;
    chapter_number_from_id(segment)
}

/// Parses a chapter number from a chapter id/segment: either `chapter-<N>-…`
/// or a bare leading number.
fn chapter_number_from_id(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("chapter-").unwrap_or(id);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn absolutize_novelarrow(href: &str) -> String {
    if href.starts_with("http") {
        href.to_string()
    } else if let Some(rest) = href.strip_prefix('/') {
        format!("https://novelarrow.com/{rest}")
    } else {
        format!("https://novelarrow.com/{href}")
    }
}

fn meta_content(html: &Html, name: &str) -> Option<String> {
    for attr in ["name", "property"] {
        let sel = format!("meta[{attr}='{name}']");
        // `let-else` drops the borrowing `SelectorErrorKind` immediately;
        // an `if let Ok(..)` would keep `sel` borrowed past its scope.
        let Ok(selector) = Selector::parse(&sel) else {
            continue;
        };
        if let Some(el) = html.select(&selector).next() {
            if let Some(content) = el.value().attr("content") {
                if !content.trim().is_empty() {
                    return Some(content.trim().to_string());
                }
            }
        }
    }
    None
}

fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let el = html.select(&selector).next()?;
    let text = el.text().collect::<Vec<_>>().join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Title-cases an all-caps genre token ("SCI-FI" → "Sci-Fi").
fn title_case(value: &str) -> String {
    value
        .split(['-', ' '])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTERS_HTML: &str = r#"
    <html><head>
      <meta name="og:novel:novel_name" content="My Gene Evolves Infinitely"/>
      <meta name="og:novel:author" content="Blue Sky Washing Rain"/>
      <meta name="og:novel:status" content="Completed"/>
      <meta name="og:novel:genre" content="SCI-FI,ACTION,FANTASY"/>
      <meta name="og:image" content="https://images.novelarrow.com/x.webp"/>
    </head><body>
      <a href="/chapter/my-gene-evolves-infinitely/chapter-2-b">Chapter 2</a>
      <a href="/chapter/my-gene-evolves-infinitely/chapter-1-a">Chapter 1</a>
      <a href="/chapter/my-gene-evolves-infinitely/chapter-10-c">Chapter 10</a>
      <a href="/chapter/my-gene-evolves-infinitely/chapter-1-a">Chapter 1 dup</a>
      <a href="/novel/other">unrelated</a>
    </body></html>"#;

    // A novel whose chapters use the bare `/chapter/<slug>/<N>` URL shape.
    const NUMERIC_HTML: &str = r#"
    <html><body>
      <a href="/chapter/god-crafter/2">Chapter 2: Harsh Lessons</a>
      <a href="/chapter/god-crafter/1">Shadows of a borrowed life</a>
      <a href="/chapter/god-crafter/10">Chapter 10</a>
      <a href="/chapter/other-novel/5">wrong slug</a>
    </body></html>"#;

    #[test]
    fn parses_and_sorts_chapters() {
        let html = Html::parse_document(CHAPTERS_HTML);
        let chapters = parse_chapter_links(&html, "my-gene-evolves-infinitely");
        assert_eq!(chapters.len(), 3);
        assert_eq!(
            chapters[0].url,
            "https://novelarrow.com/chapter/my-gene-evolves-infinitely/chapter-1-a"
        );
        assert!(chapters[2].url.contains("chapter-10-c"));
    }

    #[test]
    fn parses_numeric_chapter_urls() {
        let html = Html::parse_document(NUMERIC_HTML);
        let chapters = parse_chapter_links(&html, "god-crafter");
        assert_eq!(chapters.len(), 3);
        assert_eq!(
            chapters[0].url,
            "https://novelarrow.com/chapter/god-crafter/1"
        );
        assert_eq!(
            chapters[2].url,
            "https://novelarrow.com/chapter/god-crafter/10"
        );
    }

    #[test]
    fn reads_metadata() {
        let html = Html::parse_document(CHAPTERS_HTML);
        assert_eq!(
            meta_content(&html, "og:novel:novel_name").as_deref(),
            Some("My Gene Evolves Infinitely")
        );
        assert_eq!(title_case("SCI-FI"), "Sci Fi");
    }

    #[test]
    fn chapter_number_parsing() {
        let prefix = "/chapter/x/";
        assert_eq!(
            chapter_number_for("/chapter/x/chapter-780-final", prefix),
            Some(780)
        );
        assert_eq!(
            chapter_number_for("/chapter/x/chapter-1-a", prefix),
            Some(1)
        );
        assert_eq!(chapter_number_for("/chapter/x/53", prefix), Some(53));
        assert_eq!(
            chapter_number_for("/chapter/x/7?restore=1", prefix),
            Some(7)
        );
        assert_eq!(chapter_number_for("/novel/x", prefix), None);
    }

    #[test]
    fn rsc_chapter_fallback() {
        // Positionally-aligned chapter_id / chapter_name arrays in the RSC.
        let raw = concat!(
            "<script>self.__next_f.push([1,\"2:[",
            "{\\\"chapter_id\\\":\\\"1\\\",\\\"chapter_name\\\":\\\"Prologue\\\"},",
            "{\\\"chapter_id\\\":\\\"2\\\",\\\"chapter_name\\\":\\\"Chapter 2: Start\\\"}",
            "]\"])</script>"
        );
        let chapters = parse_rsc_chapters(raw, "god-crafter");
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].url,
            "https://novelarrow.com/chapter/god-crafter/1"
        );
        assert_eq!(chapters[0].title, "Prologue");
        assert_eq!(chapters[1].title, "Chapter 2: Start");
    }

    #[test]
    fn slug_extraction() {
        assert_eq!(
            novel_slug("https://novelarrow.com/novel/my-gene?tab=chapters").as_deref(),
            Some("my-gene")
        );
    }

    #[test]
    fn dedupes_repeated_chapter_title() {
        assert_eq!(
            clean_chapter_title("C1: Awakening Chapter 1: Awakening", 1),
            "Chapter 1: Awakening"
        );
        // A bare "Chapter" prefix on filler entries must not double up.
        assert_eq!(
            clean_chapter_title("Chapter Chapter 74: Not a Chapter", 74),
            "Chapter 74: Not a Chapter"
        );
        assert_eq!(clean_chapter_title("", 5), "Chapter 5");
        assert_eq!(clean_chapter_title("Prologue", 0), "Prologue");
    }

    #[test]
    fn accepts_single_paragraph_flight_body() {
        // Author-note chapters have one short paragraph — still valid content.
        let raw = concat!(
            "<script>self.__next_f.push([1,\"1:{\\\"chapterInfo\\\":",
            "{\\\"chapter_content\\\":\\\"$7\\\"}}\\n\"])</script>",
            "<script>self.__next_f.push([1,\"7:T15,",
            "<p>Not a chapter.</p>\\n8:x\"])</script>"
        );
        let html = extract_flight_chapter(raw).expect("single paragraph is valid");
        assert_eq!(html, "<p>Not a chapter.</p>");
    }

    // Two `__next_f` pushes: chapter metadata pointing at chunk `11` via
    // `$11`, then the body chunk `11:T<hexlen>,<html>`. The body HTML is 30
    // bytes → 0x1e. Quotes inside the pushed JSON string are `\"`-escaped and
    // the chunk boundary is a `\n`, exactly like NovelArrow's real output.
    const FLIGHT_HTML: &str = concat!(
        "<html><body>",
        "<script>self.__next_f.push([1,\"1:{\\\"chapterInfo\\\":",
        "{\\\"chapter_content\\\":\\\"$11\\\"}}\\n\"])</script>",
        "<script>self.__next_f.push([1,\"11:T1e,",
        "<p>Hello world.</p><p>Bye.</p>\\n12:x\"])</script>",
        "</body></html>"
    );

    #[test]
    fn extracts_flight_chapter_body() {
        let html = extract_flight_chapter(FLIGHT_HTML).expect("body chunk should parse");
        assert_eq!(html, "<p>Hello world.</p><p>Bye.</p>");
    }

    #[test]
    fn rejects_flight_without_prose() {
        let raw = "<script>self.__next_f.push([1,\"3:{\\\"x\\\":1}\"])</script>";
        assert!(extract_flight_chapter(raw).is_none());
    }

    #[test]
    fn extracts_inline_premium_chapter_body() {
        // Premium chapters carry their (excerpt) HTML inline in
        // `chapter_content` instead of a `$<id>` chunk reference.
        let raw = concat!(
            "<script>self.__next_f.push([1,\"9:{\\\"premium_content\\\":true,",
            "\\\"chapter_content\\\":\\\"<p><strong>Chapter 31</strong></p>",
            "<p>He tapped the treasure icon.</p>\\\"}\"])</script>"
        );
        let html = extract_flight_chapter(raw).expect("inline body should parse");
        assert!(html.contains("<p><strong>Chapter 31</strong></p>"));
        assert!(html.contains("tapped the treasure icon"));
    }
}
