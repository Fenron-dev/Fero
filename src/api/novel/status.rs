//! Reading a serial's life cycle off its NovelUpdates page.
//!
//! Answers the question a reader actually has before starting a novel: is this
//! still alive, is it finished, and is the translation likely to disappear?
//!
//! Parsing lives here rather than in the series-page parser because it is
//! answered on a different schedule — a status check runs at most weekly, while
//! chapters are fetched whenever the user asks.

use scraper::{Html, Selector};

/// What the original work is doing, as NovelUpdates reports it under
/// "Status in COO" (country of origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OriginalStatus {
    /// Still being written.
    Ongoing,
    /// Finished in the original language.
    Completed,
    /// Paused — no new chapters for a while.
    Hiatus,
    /// Abandoned by the author.
    Dropped,
    /// The page said nothing usable.
    #[default]
    Unknown,
}

/// The life-cycle facts a NovelUpdates series page exposes.
///
/// Deliberately raw: every field is what the page said, not what it means. The
/// interpretation happens in one place, so the rule stays visible and testable.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeriesStatusFacts {
    /// Status of the original work.
    pub original: OriginalStatus,
    /// Whether the translation is finished, when the page says so.
    pub fully_translated: Option<bool>,
    /// Whether the series has been licensed.
    ///
    /// The early warning that matters: once a novel is licensed, fan
    /// translations are usually taken down, often within days.
    pub licensed: Option<bool>,
    /// Highest chapter number in the release table.
    pub latest_chapter: Option<u32>,
}

/// Parses the life-cycle facts out of a NovelUpdates series page.
///
/// Missing fields stay `None`/`Unknown` rather than being guessed — a status
/// engine that invents "completed" would stop downloading a living serial.
pub fn parse_status(body: &str) -> SeriesStatusFacts {
    let html = Html::parse_document(body);
    SeriesStatusFacts {
        original: first_text(&html, "#editstatus")
            .map(|text| classify_original(&text))
            .unwrap_or_default(),
        fully_translated: first_text(&html, "#showtranslated").and_then(|text| parse_yes_no(&text)),
        licensed: first_text(&html, "#showlicensed").and_then(|text| parse_yes_no(&text)),
        latest_chapter: highest_release_number(&html),
    }
}

/// Maps the free-text "Status in COO" field onto a status.
///
/// The field is written by users, so it holds things like "142 Chapters
/// (Completed)" or "Ongoing (hiatus since 2021)". Order matters: a text
/// mentioning both a hiatus and completion describes a paused serial.
fn classify_original(text: &str) -> OriginalStatus {
    let lower = text.to_lowercase();
    if lower.contains("hiatus") {
        OriginalStatus::Hiatus
    } else if lower.contains("dropped") || lower.contains("abandoned") {
        OriginalStatus::Dropped
    } else if lower.contains("complete") {
        OriginalStatus::Completed
    } else if lower.contains("ongoing") {
        OriginalStatus::Ongoing
    } else {
        OriginalStatus::Unknown
    }
}

/// Reads a yes/no field; anything else is "no answer".
fn parse_yes_no(text: &str) -> Option<bool> {
    let lower = text.trim().to_lowercase();
    if lower.starts_with("yes") {
        Some(true)
    } else if lower.starts_with("no") {
        Some(false)
    } else {
        None
    }
}

/// Highest chapter number found in the release table.
///
/// Release titles look like "c142", "v3c7" or "Chapter 88". Only the chapter
/// part is taken: a volume number would otherwise outrank every chapter.
fn highest_release_number(html: &Html) -> Option<u32> {
    let selector = Selector::parse("a.chp-release").ok()?;
    html.select(&selector)
        .filter_map(|link| chapter_number(&link.text().collect::<String>()))
        .max()
}

/// Extracts the chapter number from a release title.
fn chapter_number(text: &str) -> Option<u32> {
    let lower = text.to_lowercase();
    // "v3c7" — take what follows the chapter marker, never the volume.
    if let Some(rest) = lower.rsplit_once('c') {
        if let Some(number) = leading_number(rest.1) {
            return Some(number);
        }
    }
    if let Some(rest) = lower.split_once("chapter ") {
        if let Some(number) = leading_number(rest.1) {
            return Some(number);
        }
    }
    None
}

/// Leading run of digits, ignoring a decimal part ("7.5" → 7).
fn leading_number(text: &str) -> Option<u32> {
    let digits: String = text
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// First matching element's text, trimmed.
fn first_text(html: &Html, raw_selector: &str) -> Option<String> {
    let selector = Selector::parse(raw_selector).ok()?;
    let text = html
        .select(&selector)
        .next()?
        .text()
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Fetches a NovelUpdates series page and reads its life-cycle facts.
///
/// Goes through the polite client like every other request, so the status
/// check shares the per-host rate limit with chapter downloads instead of
/// hammering the site alongside them.
///
/// # Errors
/// [`FeroError::ExternalApi`] when the page cannot be fetched.
pub fn fetch_status(
    client: &super::PoliteClient,
    url: &str,
) -> crate::error::Result<SeriesStatusFacts> {
    let (_, body) = client.get_text(url)?;
    Ok(parse_status(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(status: &str, translated: &str, licensed: &str, releases: &[&str]) -> String {
        let rows: String = releases
            .iter()
            .map(|title| format!(r#"<a class="chp-release">{title}</a>"#))
            .collect();
        format!(
            r#"<html><body>
                <div id="editstatus">{status}</div>
                <div id="showtranslated">{translated}</div>
                <div id="showlicensed">{licensed}</div>
                {rows}
            </body></html>"#
        )
    }

    #[test]
    fn reads_all_four_signals() {
        let facts = parse_status(&page("Completed", "Yes", "No", &["c1", "c142"]));

        assert_eq!(
            facts,
            SeriesStatusFacts {
                original: OriginalStatus::Completed,
                fully_translated: Some(true),
                licensed: Some(false),
                latest_chapter: Some(142),
            }
        );
    }

    /// The field is free text written by users, not a dropdown.
    #[test]
    fn classifies_free_text_status() {
        assert_eq!(
            classify_original("142 Chapters (Completed)"),
            OriginalStatus::Completed
        );
        assert_eq!(classify_original("Ongoing"), OriginalStatus::Ongoing);
        assert_eq!(classify_original("Dropped"), OriginalStatus::Dropped);
    }

    /// A serial that was completed and then paused is paused — the hiatus is
    /// the newer fact, so it wins over the completion mentioned alongside it.
    #[test]
    fn hiatus_wins_over_completion() {
        assert_eq!(
            classify_original("Completed (Hiatus since 2021)"),
            OriginalStatus::Hiatus
        );
    }

    #[test]
    fn unknown_status_is_not_guessed() {
        assert_eq!(classify_original("N/A"), OriginalStatus::Unknown);
        let facts = parse_status("<html><body></body></html>");
        assert_eq!(facts.original, OriginalStatus::Unknown);
        assert_eq!(facts.fully_translated, None);
        assert_eq!(facts.licensed, None);
    }

    /// A volume number must never be read as a chapter number.
    #[test]
    fn volume_prefix_does_not_win() {
        assert_eq!(chapter_number("v3c7"), Some(7));
        assert_eq!(chapter_number("v12c204"), Some(204));
    }

    #[test]
    fn reads_spelled_out_and_decimal_chapters() {
        assert_eq!(chapter_number("Chapter 88"), Some(88));
        assert_eq!(chapter_number("c7.5"), Some(7));
        assert_eq!(chapter_number("Side Story"), None);
    }

    #[test]
    fn latest_chapter_is_the_highest_not_the_last() {
        // The table lists newest first, but the order must not matter.
        let facts = parse_status(&page("Ongoing", "No", "No", &["c9", "c142", "c30"]));
        assert_eq!(facts.latest_chapter, Some(142));
    }
}
