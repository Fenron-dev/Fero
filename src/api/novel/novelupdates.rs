//! # api::novel::novelupdates
//!
//! NovelUpdates "update radar" adapter (`novelupdates.com`).
//!
//! NovelUpdates hosts no chapter text itself — it aggregates release links
//! that redirect to the translator's site.  This adapter parses the series
//! page for metadata plus the release table, and downloads chapter content
//! by following the release redirect and running the generic content
//! extraction on the target page.
//!
//! ## Caveats
//! NovelUpdates sits behind Cloudflare; automated access may be challenged.
//! Failures surface as a clear German error message on the subscription card
//! rather than being retried aggressively.
//!
//! ## Dependencies:
//! - `api::novel::generic` – content extraction on translator hosts

use scraper::{Html, Selector};

use super::{
    absolutize, generic::extract_best_content, ChapterContent, ChapterRef, NovelInfo, NovelSource,
    PoliteClient,
};
use crate::error::{Result, VaultError};

/// NovelUpdates radar adapter.
pub struct NovelUpdatesSource;

impl NovelSource for NovelUpdatesSource {
    fn id(&self) -> &'static str {
        "novelupdates"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (final_url, body) = client.get_text(url).map_err(with_translator_hint)?;
        parse_series_page(&final_url, &body).map_err(with_translator_hint)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        // The release link is a NU redirect; get_text follows it and reports
        // the final translator-host URL, which drives per-host rate limiting.
        let (final_url, body) = client.get_text(&chapter.url)?;
        let content = extract_best_content(&body).ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "Kapitelinhalt auf der Übersetzer-Seite nicht erkannt: {final_url}"
            ))
        })?;
        Ok(ChapterContent {
            title: chapter.title.clone(),
            xhtml: content,
        })
    }
}

/// Appends a German hint to NU failures: subscribing at the translator's own
/// site works via the generic parser and is the reliable path.
fn with_translator_hint(error: VaultError) -> VaultError {
    VaultError::ExternalApi(format!(
        "{error} — Tipp: Abonniere stattdessen direkt die Seite der \
         Übersetzer-Gruppe (Link in der NovelUpdates-Release-Liste); \
         diese funktioniert meist über den generischen Parser."
    ))
}

/// Parses a NovelUpdates series page: title, description, release table.
fn parse_series_page(page_url: &str, body: &str) -> Result<NovelInfo> {
    let html = Html::parse_document(body);

    let title = first_text(&html, ".seriestitlenu")
        .or_else(|| first_text(&html, ".seriestitle"))
        .or_else(|| first_text(&html, "h1"))
        .ok_or_else(|| {
            VaultError::ExternalApi(format!(
                "NovelUpdates-Serientitel nicht gefunden (Cloudflare-Block?): {page_url}"
            ))
        })?;
    let description = first_text(&html, "#editdescription");
    let author = first_text(&html, "#showauthors a").or_else(|| first_text(&html, "#showauthors"));
    let cover_url = first_attr(&html, ".seriesimg img", "src")
        .or_else(|| first_attr(&html, ".wpb_wrapper img", "src"))
        .map(|src| absolutize(page_url, &src));
    // "Status in COO" / translation status contains "Completed" for finished series.
    let completed_hint =
        first_text(&html, "#showtranslated").map(|status| status.to_lowercase().contains("yes"));

    let row_selector = Selector::parse("table#myTable tr")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;
    let link_selector = Selector::parse("a.chp-release")
        .map_err(|e| VaultError::ExternalApi(format!("selector parse error: {e}")))?;

    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in html.select(&row_selector) {
        for link in row.select(&link_selector) {
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
    }

    if chapters.is_empty() {
        return Err(VaultError::ExternalApi(format!(
            "Keine Releases auf der NovelUpdates-Seite gefunden: {page_url}"
        )));
    }

    // The release table lists newest first; adapters must return oldest first.
    chapters.reverse();

    Ok(NovelInfo {
        title,
        author,
        cover_url,
        description,
        completed_hint,
        genres: collect_texts(&html, "#seriesgenre a"),
        tags: collect_texts(&html, "#showtags a"),
        chapters,
    })
}

fn collect_texts(html: &Html, raw_selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(raw_selector) else {
        return Vec::new();
    };
    html.select(&selector)
        .map(|element| {
            let text = element.text().collect::<Vec<_>>().join(" ");
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|text| !text.is_empty())
        .collect()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES_PAGE: &str = r#"
    <html><body>
      <div class="seriestitlenu">Reverend Testsanity</div>
      <div id="editdescription"><p>A cultivation story.</p></div>
      <div id="showauthors"><a href="/nauthor/x/">Er Gen</a></div>
      <table id="myTable">
        <tr><td>Group</td><td><a class="chp-release" href="//www.novelupdates.com/extnu/999/">c3</a></td></tr>
        <tr><td>Group</td><td><a class="chp-release" href="//www.novelupdates.com/extnu/998/">c2</a></td></tr>
        <tr><td>Group</td><td><a class="chp-release" href="//www.novelupdates.com/extnu/997/">c1</a></td></tr>
      </table>
    </body></html>"#;

    #[test]
    fn parses_series_page_and_reverses_releases() {
        let info = parse_series_page(
            "https://www.novelupdates.com/series/reverend-testsanity/",
            SERIES_PAGE,
        )
        .expect("page should parse");
        assert_eq!(info.title, "Reverend Testsanity");
        assert_eq!(info.author.as_deref(), Some("Er Gen"));
        assert_eq!(info.chapters.len(), 3);
        // Oldest first after the reverse.
        assert_eq!(info.chapters[0].title, "c1");
        assert_eq!(info.chapters[2].title, "c3");
        assert_eq!(
            info.chapters[0].url,
            "https://www.novelupdates.com/extnu/997/"
        );
    }

    #[test]
    fn fails_cleanly_on_challenge_page() {
        let result = parse_series_page(
            "https://www.novelupdates.com/series/x/",
            "<html><body>Checking your browser…</body></html>",
        );
        assert!(result.is_err());
    }
}
