//! # api::manga::fanfox
//!
//! FanFox adapter (`fanfox.net`, mirror `mangafox.la`).
//!
//! ## Page model (DM5 reader)
//! Image URLs are not in the chapter page.  The page ships a packed script
//! that writes a per-chapter key into `#dm5_key`; the reader then asks
//! `chapterfun.ashx?cid=<chapter>&page=<n>&key=<key>` for the URLs of the next
//! two pages, again as a packed script.  This adapter walks the same route:
//!
//! 1. chapter page → `chapterid`, `imagecount`, packed key
//! 2. `chapterfun.ashx` per page pair → packed `pix` + `pvalue`
//! 3. `pix` (base path) + each `pvalue` entry (file + token) = the image URL
//!
//! The image tokens expire, so URLs are resolved right before downloading
//! rather than cached in the subscription record.
//!
//! ## The `isAdult` cookie
//! Without it the series page renders only its newest chapter.  Sending it is
//! what makes the full chapter list visible, exactly as clicking the site's
//! own age gate does.
//!
//! ## Dependencies:
//! - `api::manga::packed` – decoder for the packed scripts
//! - `api::novel` – shared HTTP client and URL helpers

use scraper::{Html, Selector};

use super::{
    extract_chapter_number, extract_volume, packed, tidy_text, MangaChapterRef, MangaInfo,
    MangaSource, PageImage,
};
use crate::api::novel::{absolutize, PoliteClient};
use crate::error::{Result, FeroError};

/// Header that reveals the complete chapter list.
const ADULT_HEADERS: [(&str, &str); 1] = [("Cookie", "isAdult=1")];

/// Pages returned per `chapterfun.ashx` call, as the site's own reader uses.
const PAGES_PER_CALL: u32 = 2;

/// Upper bound on `chapterfun.ashx` calls per chapter.
///
/// Guards against a malformed `imagecount` turning one chapter into an endless
/// request loop against the site.
const MAX_CALLS_PER_CHAPTER: u32 = 400;

/// FanFox adapter.
pub struct FanFoxSource;

impl MangaSource for FanFoxSource {
    fn id(&self) -> &'static str {
        "fanfox"
    }

    fn fetch_series_info(&self, client: &PoliteClient, url: &str) -> Result<MangaInfo> {
        let (final_url, body) = client.get_text_with(url, &ADULT_HEADERS)?;
        parse_series_page(&final_url, &body)
    }

    fn fetch_chapter_pages(
        &self,
        client: &PoliteClient,
        chapter: &MangaChapterRef,
    ) -> Result<Vec<PageImage>> {
        let (final_url, body) = client.get_text_with(&chapter.url, &ADULT_HEADERS)?;
        let context = parse_chapter_context(&final_url, &body)?;

        let mut images: Vec<PageImage> = Vec::with_capacity(context.image_count as usize);
        let mut page = 1u32;
        let mut calls = 0u32;
        while images.len() < context.image_count as usize && calls < MAX_CALLS_PER_CHAPTER {
            let request_url = format!(
                "{}chapterfun.ashx?cid={}&page={page}&key={}",
                context.base_url, context.chapter_id, context.key
            );
            let (_, script) = client.get_text_with(
                &request_url,
                &[("Cookie", "isAdult=1"), ("Referer", &final_url)],
            )?;

            let batch = parse_page_batch(&context.base_url, &script)?;
            if batch.is_empty() {
                break;
            }
            for url in batch {
                if images.len() >= context.image_count as usize {
                    break;
                }
                images.push(PageImage::with_referer(url, final_url.clone()));
            }
            page += PAGES_PER_CALL;
            calls += 1;
        }

        if images.is_empty() {
            return Err(FeroError::ExternalApi(format!(
                "Keine Seitenbilder ermittelt: {}",
                chapter.url
            )));
        }
        Ok(images)
    }
}

/// Everything needed to drive the `chapterfun.ashx` calls of one chapter.
#[derive(Debug, PartialEq, Eq)]
struct ChapterContext {
    /// Chapter URL up to and including the trailing slash.
    base_url: String,
    /// Numeric chapter id (`var chapterid = …`).
    chapter_id: String,
    /// Number of pages (`var imagecount = …`).
    image_count: u32,
    /// Per-chapter key, decoded from the packed script.
    key: String,
}

/// Parses a series page: metadata plus the full chapter list.
fn parse_series_page(page_url: &str, body: &str) -> Result<MangaInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, ".detail-info-right-title-font")
        .or_else(|| first_text(&html, "h1"))
        .ok_or_else(|| {
            FeroError::ExternalApi(format!(
                "FanFox-Titel nicht gefunden (Cloudflare-Block?): {page_url}"
            ))
        })?;

    let link_selector = Selector::parse("ul.detail-main-list a")
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
        // `title` carries the clean label; the element text is padded markup.
        let label = link
            .value()
            .attr("title")
            .map(tidy_text)
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
            "Keine Kapitel auf der FanFox-Seite gefunden: {page_url}"
        )));
    }
    // The site lists newest first; adapters must return oldest first.
    chapters.reverse();

    let status_text = first_text(&html, ".detail-info-right-title-tip").unwrap_or_default();
    Ok(MangaInfo {
        title,
        author: first_text(&html, ".detail-info-right-say a"),
        artist: None,
        cover_url: first_attr(&html, ".detail-info-cover-img", "src")
            .map(|src| absolutize(page_url, &src)),
        description: first_text(&html, ".fullcontent")
            .or_else(|| first_text(&html, ".detail-info-right-content")),
        completed_hint: Some(status_text.to_lowercase().contains("completed")),
        genres: collect_texts(&html, ".detail-info-right-tag-list a"),
        tags: Vec::new(),
        right_to_left: true,
        chapters,
    })
}

/// Extracts chapter id, page count, and the decoded key from a chapter page.
fn parse_chapter_context(page_url: &str, body: &str) -> Result<ChapterContext> {
    let chapter_id = js_number(body, "chapterid").ok_or_else(|| {
        FeroError::ExternalApi(format!("FanFox: chapterid nicht gefunden: {page_url}"))
    })?;
    let image_count: u32 = js_number(body, "imagecount")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            FeroError::ExternalApi(format!("FanFox: imagecount nicht gefunden: {page_url}"))
        })?;
    let key = extract_key(body).ok_or_else(|| {
        FeroError::ExternalApi(format!(
            "FanFox: Kapitel-Schlüssel nicht lesbar: {page_url}"
        ))
    })?;

    Ok(ChapterContext {
        base_url: base_url_of(page_url),
        chapter_id,
        image_count,
        key,
    })
}

/// Decodes the packed script that assembles the `dm5_key` value.
///
/// The unpacked source concatenates the key one character at a time
/// (`var d=''+'9'+'9'+…`), so the digits are collected from the quoted pieces.
fn extract_key(body: &str) -> Option<String> {
    for script in packed_scripts(body) {
        let Some(unpacked) = packed::unpack(&script) else {
            continue;
        };
        if !unpacked.contains("dm5_key") {
            continue;
        }
        let assignment = unpacked.split_once('=')?.1;
        let assembled: String = assignment
            .split(';')
            .next()?
            .split('\'')
            .skip(1)
            .step_by(2)
            .collect();
        if !assembled.is_empty() {
            return Some(assembled);
        }
    }
    None
}

/// Parses one `chapterfun.ashx` response into absolute image URLs.
fn parse_page_batch(base_url: &str, script: &str) -> Result<Vec<String>> {
    let unpacked = packed::unpack(script).ok_or_else(|| {
        FeroError::ExternalApi(
            "FanFox: Antwort des Bild-Endpunkts nicht entschlüsselbar (Seite geändert?)"
                .to_string(),
        )
    })?;

    let prefix = js_string_value(&unpacked, "pix").ok_or_else(|| {
        FeroError::ExternalApi("FanFox: Bildpfad (pix) fehlt in der Antwort".to_string())
    })?;
    let values = js_array_values(&unpacked, "pvalue").ok_or_else(|| {
        FeroError::ExternalApi("FanFox: Bildliste (pvalue) fehlt in der Antwort".to_string())
    })?;

    Ok(values
        .into_iter()
        .map(|value| absolutize(base_url, &format!("{prefix}{value}")))
        .collect())
}

// ---------------------------------------------------------------------------
// Small parsing helpers
// ---------------------------------------------------------------------------

/// Returns every `eval(function(p,a,c,k,e,d)` script found in `body`.
fn packed_scripts(body: &str) -> Vec<String> {
    const MARKER: &str = "eval(function(p,a,c,k,e,d)";
    let mut scripts = Vec::new();
    let mut rest = body;
    while let Some(position) = rest.find(MARKER) {
        let candidate = &rest[position..];
        // A packed call ends at the closing `))` of the eval; taking the rest
        // of the document also works because the parser stops at the payload.
        let end = candidate.find("</script>").unwrap_or(candidate.len());
        scripts.push(candidate[..end].to_string());
        rest = &candidate[MARKER.len()..];
    }
    scripts
}

/// Reads `var <name> = <number>;` from a script body.
fn js_number(body: &str, name: &str) -> Option<String> {
    let position = body.find(&format!("var {name}"))?;
    let after = body[position..].split_once('=')?.1;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Reads `<name>="<value>"` (or single quotes) from a script body.
fn js_string_value(body: &str, name: &str) -> Option<String> {
    let position = body.find(name)?;
    let after = body[position + name.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after
        .chars()
        .next()
        .filter(|ch| *ch == '"' || *ch == '\'')?;
    let rest = &after[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Reads `<name>=["a","b"]` from a script body.
fn js_array_values(body: &str, name: &str) -> Option<Vec<String>> {
    let position = body.find(name)?;
    let after = body[position + name.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix('[')?;
    let end = after.find(']')?;
    Some(
        after[..end]
            .split(',')
            .filter_map(|item| {
                let trimmed = item.trim().trim_matches(['"', '\'']).trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect(),
    )
}

/// Strips the file name from a chapter URL, keeping the trailing slash.
fn base_url_of(url: &str) -> String {
    match url.rfind('/') {
        Some(position) => url[..=position].to_string(),
        None => url.to_string(),
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

    const PACKER_PREFIX: &str = "eval(function(p,a,c,k,e,d){e=function(c){return c};\
                                 while(c--)if(k[c])p=p.replace(x,k[c]);return p;}(";

    const SERIES_PAGE: &str = r#"
    <html><body>
      <p class="detail-info-right-title-font">Chainsaw Man</p>
      <span class="detail-info-right-title-tip">Ongoing</span>
      <img class="detail-info-cover-img" src="//static.example/cover.jpg">
      <p class="detail-info-right-say"><a href="/search/author/x/">Fujimoto</a></p>
      <p class="fullcontent">Ein Teufelsjäger.</p>
      <p class="detail-info-right-tag-list"><a href="/action/">Action</a></p>
      <ul class="detail-main-list">
        <li><a href="/manga/chainsaw_man/c003/1.html" title="Chainsaw Man Ch.003"> x </a></li>
        <li><a href="/manga/chainsaw_man/c002/1.html" title="Chainsaw Man Ch.002"> x </a></li>
        <li><a href="/manga/chainsaw_man/c001/1.html" title="Chainsaw Man Ch.001"> x </a></li>
      </ul>
    </body></html>"#;

    #[test]
    fn parses_series_metadata_and_orders_chapters_oldest_first() {
        let info = parse_series_page("https://fanfox.net/manga/chainsaw_man/", SERIES_PAGE)
            .expect("series page should parse");

        assert_eq!(info.title, "Chainsaw Man");
        assert_eq!(info.author.as_deref(), Some("Fujimoto"));
        assert_eq!(info.completed_hint, Some(false));
        assert_eq!(info.genres, vec!["Action".to_string()]);
        assert_eq!(info.chapters.len(), 3);
        assert_eq!(info.chapters[0].number.as_deref(), Some("1"));
        assert_eq!(info.chapters[2].number.as_deref(), Some("3"));
        assert_eq!(
            info.chapters[0].url,
            "https://fanfox.net/manga/chainsaw_man/c001/1.html"
        );
    }

    #[test]
    fn reads_chapter_context_including_the_packed_key() {
        // Mirrors the real chapter page: the key is assembled digit by digit.
        let body = format!(
            "<html><script>var comicid = 29295; var chapterid =568779; var imagecount=53;</script>\
             <script>{PACKER_PREFIX}'7 3=\\'\\'+\\'9\\'+\\'0\\'+\\'5\\';$(\"#a\").b(3);',\
             12,12,'||e|guidkey||||var|d||dm5_key|val'.split('|'),0,{{}}))</script></html>"
        );

        let context =
            parse_chapter_context("https://fanfox.net/manga/chainsaw_man/c001/1.html", &body)
                .expect("chapter context should parse");

        assert_eq!(context.chapter_id, "568779");
        assert_eq!(context.image_count, 53);
        assert_eq!(context.key, "905");
        assert_eq!(
            context.base_url,
            "https://fanfox.net/manga/chainsaw_man/c001/"
        );
    }

    #[test]
    fn missing_key_is_a_clear_error() {
        let body = "<html><script>var chapterid =1; var imagecount=2;</script></html>";
        let error = parse_chapter_context("https://fanfox.net/manga/x/c001/1.html", body)
            .expect_err("missing key should error");
        assert!(error.to_string().contains("Schlüssel"));
    }

    #[test]
    fn builds_image_urls_from_a_page_batch() {
        // Base 7: the `99` tokens are not valid base-7 numbers, so they stay
        // literal — the packer likewise never emits a token that collides with
        // a dictionary index it still needs.
        let script = format!(
            "{PACKER_PREFIX}'6 5(){{4 0=\"//s.example/store/x/\";4 1=[\"/a.jpg?t=99\",\"/b.jpg?t=98\"];}}',\
             7,7,'pix|pvalue|||var|fun|function'.split('|'),0,{{}}))"
        );

        let urls = parse_page_batch("https://fanfox.net/manga/x/c001/", &script)
            .expect("batch should parse");

        assert_eq!(
            urls,
            vec![
                "https://s.example/store/x//a.jpg?t=99".to_string(),
                "https://s.example/store/x//b.jpg?t=98".to_string(),
            ]
        );
    }

    #[test]
    fn unparseable_batch_reports_a_site_change() {
        let error = parse_page_batch("https://fanfox.net/manga/x/c001/", "not a packed script")
            .expect_err("garbage should error");
        assert!(error.to_string().contains("nicht entschlüsselbar"));
    }

    #[test]
    fn base_url_drops_the_page_file() {
        assert_eq!(
            base_url_of("https://fanfox.net/manga/x/c001/1.html"),
            "https://fanfox.net/manga/x/c001/"
        );
    }
}
