//! Turning the release dates on a source page into timestamps.
//!
//! Every scanlation and aggregator site prints when a chapter went up, and
//! every one of them prints it differently: "2 days ago" on one, "August 10,
//! 2026" on the next, "2026-08-10" on a third. The reader's question is the
//! same in all three cases — is this series still alive, and how long has the
//! last chapter been sitting there?
//!
//! ## Why hand-rolled
//! A date library would be a large dependency for one job: mapping a handful
//! of shapes onto a UNIX timestamp, at day resolution, without time zones.
//! There is no time zone to get right here — the sites do not say which one
//! they mean, so anything finer than a day would be false precision.
//!
//! ## What it refuses
//! Purely numeric dates (`08/10/2026`) are not parsed. `MM/DD` and `DD/MM` are
//! indistinguishable without knowing the site, and a date that is wrong by up
//! to eleven months is worse than no date at all.

/// Parses a release date into a UNIX timestamp; `None` when nothing fits.
///
/// `now` is passed in rather than read from the clock so relative dates stay
/// testable.
pub fn parse_release(text: &str, now: u64) -> Option<u64> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    parse_relative(&lower, now)
        .or_else(|| parse_iso(&lower))
        .or_else(|| parse_month_name(&lower))
}

/// Parses `MM/DD/YY` — only for callers that know their site's convention.
///
/// [`parse_release`] refuses this shape on purpose, because `08/10/26` is
/// August or October depending on who wrote it. NovelUpdates is a US site and
/// always writes month first, so there the ambiguity does not exist — but that
/// knowledge belongs to the adapter, not to the general parser, which is why
/// this is a separate door rather than a widening of the first one.
pub fn parse_month_first_short(text: &str) -> Option<u64> {
    let head: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '/')
        .collect();
    let mut parts = head.split('/');
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let year: i64 = parts.next()?.parse().ok()?;
    // Two-digit years on these tables are always this century.
    let year = if year < 100 { 2000 + year } else { year };
    to_unix(year, month, day)
}

/// "2 days ago", "an hour ago", "yesterday", "vor 3 Tagen".
fn parse_relative(lower: &str, now: u64) -> Option<u64> {
    if lower.contains("just now") || lower.contains("gerade eben") || lower.starts_with("today") {
        return Some(now);
    }
    if lower.starts_with("yesterday") || lower.starts_with("gestern") {
        return Some(now.saturating_sub(86_400));
    }
    if !lower.contains("ago") && !lower.starts_with("vor ") {
        return None;
    }

    let mut amount: Option<u64> = None;
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if let Ok(number) = word.parse::<u64>() {
            amount = Some(number);
            continue;
        }
        // "an hour ago" / "a day ago" — the article is the number one.
        if matches!(word, "a" | "an" | "one" | "einer" | "einem") {
            amount = Some(1);
            continue;
        }
        if let Some(unit) = unit_seconds(word) {
            // A unit without a number in front of it is a lone "day ago",
            // which no site writes; refuse rather than guess.
            return amount.map(|amount| now.saturating_sub(amount.saturating_mul(unit)));
        }
    }
    None
}

/// Seconds in a unit word, English or German.
///
/// Month and year are the average lengths. At the resolution this is used for
/// — "how stale is the newest chapter" — the drift does not matter, and the
/// alternative is calendar arithmetic on a date the site did not give.
fn unit_seconds(word: &str) -> Option<u64> {
    Some(match word {
        "second" | "seconds" | "sec" | "secs" | "sekunde" | "sekunden" => 1,
        "minute" | "minutes" | "min" | "mins" | "minuten" => 60,
        "hour" | "hours" | "hr" | "hrs" | "stunde" | "stunden" => 3_600,
        "day" | "days" | "tag" | "tagen" => 86_400,
        "week" | "weeks" | "woche" | "wochen" => 7 * 86_400,
        "month" | "months" | "monat" | "monaten" => 2_629_746,
        "year" | "years" | "jahr" | "jahren" => 31_556_952,
        _ => return None,
    })
}

/// `2026-08-10`, optionally followed by a time this ignores.
fn parse_iso(lower: &str) -> Option<u64> {
    let head: String = lower
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    let mut parts = head.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    to_unix(year, month, day)
}

/// `August 10, 2026`, `Aug 10 2026`, `10 August 2026`.
fn parse_month_name(lower: &str) -> Option<u64> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();

    let position = words.iter().position(|word| month_number(word).is_some())?;
    let month = month_number(words[position])?;

    // The two numbers around the month name are the day and the year; which is
    // which follows from the size, not from the order the site chose.
    let mut numbers: Vec<i64> = Vec::new();
    for offset in [
        position.checked_sub(1),
        Some(position + 1),
        Some(position + 2),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(number) = words.get(offset).and_then(|word| word.parse::<i64>().ok()) {
            numbers.push(number);
        }
    }
    let year = *numbers.iter().find(|number| **number > 31)?;
    let day = *numbers.iter().find(|number| **number <= 31)?;
    to_unix(year, month, day)
}

/// Month number for an English month name, full or three-letter.
fn month_number(word: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    MONTHS
        .iter()
        .position(|month| *month == word || (word.len() == 3 && month.starts_with(word)))
        .map(|index| index as i64 + 1)
}

/// Midnight UTC of a civil date, as a UNIX timestamp.
///
/// Howard Hinnant's `days_from_civil`. Years outside a plausible range are
/// refused: a site that prints a placeholder year should not produce a
/// timestamp that looks like an answer.
fn to_unix(year: i64, month: i64, day: i64) -> Option<u64> {
    if !(1990..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-10, 00:00 UTC.
    const TENTH_OF_AUGUST: u64 = 1_786_320_000;
    /// 2026-05-28, some time in the evening — deliberately not midnight, so a
    /// relative date cannot accidentally agree with an absolute one.
    const NOW: u64 = 1_780_000_000;

    #[test]
    fn the_same_day_written_three_ways_gives_the_same_answer() {
        assert_eq!(parse_release("2026-08-10", NOW), Some(TENTH_OF_AUGUST));
        assert_eq!(parse_release("August 10, 2026", NOW), Some(TENTH_OF_AUGUST));
        assert_eq!(parse_release("10 Aug 2026", NOW), Some(TENTH_OF_AUGUST));
        // Sites append a time; the day is all this promises.
        assert_eq!(
            parse_release("2026-08-10 14:22:01", NOW),
            Some(TENTH_OF_AUGUST)
        );
    }

    #[test]
    fn relative_dates_count_backwards_from_now() {
        assert_eq!(parse_release("2 days ago", NOW), Some(NOW - 2 * 86_400));
        assert_eq!(parse_release("an hour ago", NOW), Some(NOW - 3_600));
        assert_eq!(parse_release("30 mins ago", NOW), Some(NOW - 1_800));
        assert_eq!(parse_release("vor 3 Tagen", NOW), Some(NOW - 3 * 86_400));
        assert_eq!(parse_release("yesterday", NOW), Some(NOW - 86_400));
        assert_eq!(parse_release("just now", NOW), Some(NOW));
    }

    /// `08/10/2026` is October or August depending on the site, and nothing in
    /// the string says which. A date wrong by months is worse than none.
    #[test]
    fn ambiguous_and_unparseable_text_yields_nothing() {
        assert_eq!(parse_release("08/10/2026", NOW), None);
        assert_eq!(parse_release("", NOW), None);
        assert_eq!(parse_release("Chapter 12", NOW), None);
        assert_eq!(parse_release("New", NOW), None);
    }

    /// NovelUpdates writes `MM/DD/YY`, and the adapter knows that even though
    /// the general parser must not assume it.
    #[test]
    fn the_month_first_door_resolves_what_the_general_parser_refuses() {
        assert_eq!(parse_month_first_short("08/10/26"), Some(TENTH_OF_AUGUST));
        assert_eq!(parse_month_first_short("08/10/2026"), Some(TENTH_OF_AUGUST));
        assert_eq!(parse_month_first_short("13/10/26"), None);
        assert_eq!(parse_month_first_short("nichts"), None);
        // Dieselbe Zeichenkette bleibt fuer den allgemeinen Weg mehrdeutig.
        assert_eq!(parse_release("08/10/26", NOW), None);
    }

    /// A placeholder year must not turn into a timestamp that looks real.
    #[test]
    fn implausible_years_are_refused() {
        assert_eq!(parse_release("1900-01-01", NOW), None);
        assert_eq!(parse_release("January 1, 1899", NOW), None);
        assert_eq!(parse_release("2026-13-01", NOW), None);
    }

    /// The epoch conversion has to agree with known dates, or every displayed
    /// date is quietly off.
    #[test]
    fn known_dates_convert_exactly() {
        assert_eq!(to_unix(2000, 1, 1), Some(946_684_800));
        assert_eq!(to_unix(2024, 2, 29), Some(1_709_164_800));
        assert_eq!(to_unix(2026, 12, 31), Some(1_798_675_200));
    }
}
