//! Deciding what a serial's status *means*.
//!
//! The reading of a source page happens in `api::novel::status` (NovelUpdates)
//! and `api::manga::status` (AniList/MyAnimeList); this module turns those raw
//! facts into the two decisions that change behaviour: is the serial finished,
//! so the complete edition can be built — and how often is it still worth
//! looking for new chapters?
//!
//! Kept apart from the parsing on purpose. The rule is a product decision and
//! gets argued about; the parsing is a fact about someone else's HTML.

use serde::{Deserialize, Serialize};

use crate::api::novel::status::{OriginalStatus, SeriesStatusFacts};

/// How a serial stands, as far as Fero can tell.
///
/// The wire values are German on purpose: this is the one enum that leaves
/// Fero verbatim — into the subscription file, into `fero.info.json`, into
/// the API — for Fundus to read directly, and it reads exactly the six words
/// a person picking this apart would use. `#[serde(alias = "…")]` keeps a
/// subscription written before 09/2026 (English wire values, plus a
/// `"licensed"` that was a status back then) loading correctly; every fresh
/// write uses the new name. Losing that would mean an upgrade makes existing
/// subscriptions unparsable — and an unparsable one does not error, it just
/// silently disappears from the list (`list_subscriptions` skips what it
/// cannot read), which is worse than any wrong label.
///
/// `Licensed` is deliberately not a variant here any more — see
/// [`Subscription::licensed`](crate::core::subscription::Subscription::licensed):
/// it is a fact that coexists with any of these six, not a seventh one that
/// excludes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SeriesStatus {
    /// Still receiving chapters.
    #[serde(rename = "laufend", alias = "ongoing", alias = "licensed")]
    Ongoing,
    /// Finished: original done, translation done, and everything is here.
    #[serde(rename = "abgeschlossen", alias = "completed")]
    Completed,
    /// Paused upstream, no announced end — a fact about the *series*.
    #[serde(rename = "hiatus")]
    Hiatus,
    /// Abandoned upstream.
    #[serde(rename = "abgebrochen", alias = "dropped")]
    Dropped,
    /// The user paused this *subscription* — Fero currently checks nothing
    /// here. Not a fact about the series (a paused subscription can be
    /// running fine upstream); wins over every other value the moment
    /// `enabled` is false, computed in [`effective`] rather than stored.
    #[serde(rename = "pausiert")]
    Paused,
    /// Not determined yet — or genuinely ambiguous: the source gives no
    /// reliable signal for whether the serial is still continuing or was
    /// quietly dropped.
    #[default]
    #[serde(rename = "unbekannt", alias = "unknown")]
    Unknown,
}

impl SeriesStatus {
    /// Whether this status warrants telling the user about it unprompted.
    ///
    /// The two that change what someone would do: a dropped one will never
    /// finish, a paused-upstream one is not broken but idle. `Licensed` used
    /// to be a third — see [`Subscription::licensed`]; a caller that needs
    /// that signal now checks it directly rather than through the status.
    ///
    /// [`Subscription::licensed`]: crate::core::subscription::Subscription::licensed
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Dropped | Self::Hiatus)
    }

    /// Whether periodic checks can be skipped.
    ///
    /// Only a finished serial qualifies. A dropped one might still get picked
    /// up by another translator.
    pub fn is_settled(self) -> bool {
        self == Self::Completed
    }

    /// Whether this status puts a serial in the slow lane — still checked,
    /// just rarely.
    ///
    /// The three statuses that mean "nothing is coming, probably". *Probably*
    /// is why they are checked at all: translators pick a dropped series up
    /// years later, a hiatus ends, and a finished work grows a sequel arc in
    /// the same entry. A licensed serial staying in the fast lane — the one
    /// status where a missed run costs chapters that never come back — is now
    /// the caller's job: see [`should_check`], which takes that as its own flag.
    pub fn checks_rarely(self) -> bool {
        matches!(self, Self::Completed | Self::Dropped | Self::Hiatus)
    }

    /// The wire name, matching the `serde` representation.
    ///
    /// Needed because the status travels through the API as a string the user
    /// picks in a dropdown, and it has to come back in.
    pub fn as_id(self) -> &'static str {
        match self {
            Self::Ongoing => "laufend",
            Self::Completed => "abgeschlossen",
            Self::Hiatus => "hiatus",
            Self::Dropped => "abgebrochen",
            Self::Paused => "pausiert",
            Self::Unknown => "unbekannt",
        }
    }

    /// Parses a wire name back into a status; `None` for anything else.
    ///
    /// Only the current names — this is for parsing *fresh* input (a
    /// dropdown choice, a saved alias), which never sends the old English
    /// ids. Old on-disk *files* go through `serde`'s own `alias`, not this.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "laufend" => Some(Self::Ongoing),
            "abgeschlossen" => Some(Self::Completed),
            "hiatus" => Some(Self::Hiatus),
            "abgebrochen" => Some(Self::Dropped),
            "pausiert" => Some(Self::Paused),
            "unbekannt" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// How long a status stays fresh.
///
/// A serial's life cycle changes on the scale of months, so a weekly look is
/// generous. The point is not freshness but restraint: a hundred subscriptions
/// checking on every run would be a hundred extra requests for information that
/// almost never moved.
pub const STATUS_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// Whether a status check is due.
///
/// Never checked before counts as due. `now` is passed in rather than read from
/// the clock so the decision stays testable.
pub fn is_due(checked_at: Option<u64>, now: u64) -> bool {
    match checked_at {
        None => true,
        Some(then) => now.saturating_sub(then) >= STATUS_MAX_AGE_SECS,
    }
}

/// How long a serial in the slow lane rests between looks.
///
/// "Finished" is not a closed book. Translation groups pick a dropped series up
/// years later, a hiatus ends without announcement, and some sources hang a
/// sequel arc off the same entry. Never looking again would lose exactly those;
/// looking every run wastes a request on a work that almost certainly did not
/// move. Twelve looks a year is the compromise.
pub const IDLE_RECHECK_SECS: u64 = 30 * 24 * 60 * 60;

/// Whether a periodic run should look at this serial at all.
///
/// Replaces the older rule "a finished serial is never looked at again", which
/// was wrong in both directions: it dropped paused serials entirely — the very
/// case where waiting for a restart is the whole point — and it turned
/// "finished" into a verdict nothing could overturn.
///
/// `licensed` is checked before the status at all: it is the one fact where a
/// missed run costs chapters that never come back, regardless of whether the
/// serial otherwise looks settled.
///
/// A single subscription checked by hand does not come through here: an
/// explicit click always runs, whatever the status says.
pub fn should_check(
    status: SeriesStatus,
    licensed: bool,
    enabled: bool,
    last_check: Option<u64>,
    now: u64,
) -> bool {
    if !enabled {
        return false;
    }
    if licensed || !status.checks_rarely() {
        return true;
    }
    match last_check {
        None => true,
        Some(then) => now.saturating_sub(then) >= IDLE_RECHECK_SECS,
    }
}

/// The status that actually applies, out of the four that can disagree.
///
/// Precedence, strongest first: whether the *subscription* is paused, what
/// the user set by hand for the *series*, what the status source last said,
/// and finally the `completed`/`hiatus`/`dropped` flags. A paused subscription
/// wins outright and unconditionally — Fero currently checks nothing here, so
/// nothing else in this list is even being kept fresh. Below that, the hand
/// setting has to win over the source — otherwise the next check run silently
/// undoes it, which is what happened while `completed` served as both the
/// user's switch and the scraper's output.
pub fn effective(
    enabled: bool,
    manual: Option<SeriesStatus>,
    detected: SeriesStatus,
    completed: bool,
    hiatus: bool,
    dropped: bool,
) -> SeriesStatus {
    if !enabled {
        return SeriesStatus::Paused;
    }
    if let Some(manual) = manual {
        return manual;
    }
    if detected != SeriesStatus::Unknown {
        return detected;
    }
    if completed {
        SeriesStatus::Completed
    } else if hiatus {
        SeriesStatus::Hiatus
    } else if dropped {
        SeriesStatus::Dropped
    } else {
        SeriesStatus::Unknown
    }
}

/// Reads a life-cycle status out of free-form text a source printed on its own
/// page — "Ongoing", "On Hold", "Dropped by Group", "Complete", …
///
/// This is deliberately separate from [`resolve`] and [`resolve_comic`]: those
/// two combine several *structured* facts (a database's publication state,
/// whether every listed chapter is here); this one guesses at a single string
/// nobody agreed on a vocabulary for. Two sites both saying "the story is
/// finished" write it as "Completed" and "Complete" and "Finished" — and one
/// stalled indefinitely writes "Hiatus", "On Hold", or "Paused". Whatever a
/// caller cannot place here either is not covered yet, or is source-specific
/// enough that guessing would be worse than asking — see
/// `core::status_aliases` for what happens with those.
///
/// Checked in this order because a real page can combine several of these
/// words ("no longer on hiatus, fully completed") and the rarer, more specific
/// word should win over a generic one that happens to appear alongside it.
pub fn classify_status_text(text: &str) -> Option<SeriesStatus> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    const HIATUS: [&str; 5] = ["hiatus", "on hold", "on-hold", "paused", "pausiert"];
    const DROPPED: [&str; 6] = [
        "dropped",
        "cancelled",
        "canceled",
        "discontinued",
        "abandoned",
        "abgebrochen",
    ];
    const COMPLETED: [&str; 5] = [
        "completed",
        "complete",
        "finished",
        "ended",
        "abgeschlossen",
    ];
    const ONGOING: [&str; 6] = [
        "ongoing",
        "on-going",
        "publishing",
        "releasing",
        "active",
        "laufend",
    ];

    if HIATUS.iter().any(|word| lower.contains(word)) {
        Some(SeriesStatus::Hiatus)
    } else if DROPPED.iter().any(|word| lower.contains(word)) {
        Some(SeriesStatus::Dropped)
    } else if COMPLETED.iter().any(|word| lower.contains(word)) {
        Some(SeriesStatus::Completed)
    } else if ONGOING.iter().any(|word| lower.contains(word)) {
        Some(SeriesStatus::Ongoing)
    } else {
        None
    }
}

/// The life-cycle facts available for a comic, before interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComicStatusFacts {
    /// Publication status of the original work, per AniList/MyAnimeList.
    pub publication: OriginalStatus,
    /// Whether the scanlation site itself marks the series as finished.
    pub source_completed: Option<bool>,
}

/// Decides the status of a comic.
///
/// Unlike a translated novel there is no separate translation state to ask
/// about — no comic database tracks scanlation progress. What stands in for it
/// is the scanlation site's own label, plus the one fact Fero owns: whether
/// anything in the site's chapter list is still undownloaded.
///
/// The database outranks the site on the two negative statuses. A site that
/// lists a series as "Completed" is usually saying "we stopped", which is what
/// the database calls dropped or paused — and those keep a serial out of the
/// "finished, build the complete edition" bucket.
pub fn resolve_comic(facts: &ComicStatusFacts, pending_chapters: usize) -> SeriesStatus {
    match facts.publication {
        OriginalStatus::Dropped => return SeriesStatus::Dropped,
        OriginalStatus::Hiatus => return SeriesStatus::Hiatus,
        _ => {}
    }

    let finished =
        facts.publication == OriginalStatus::Completed || facts.source_completed == Some(true);
    if !finished {
        return match facts.publication {
            OriginalStatus::Ongoing => SeriesStatus::Ongoing,
            _ => SeriesStatus::Unknown,
        };
    }

    // Finished upstream but chapters still missing here: the slow lane would
    // stop fetching exactly the ones that are left.
    if pending_chapters == 0 {
        SeriesStatus::Completed
    } else {
        SeriesStatus::Ongoing
    }
}

/// Decides the status of a serial from the source facts and what is on disk.
///
/// "Completed" needs all three: the original finished, the translation
/// finished, and every listed chapter present locally. Two out of three is a
/// serial that is still going to grow — declaring it done would build a
/// "complete" edition that is missing the ending.
///
/// `local_last_chapter` is the highest chapter number Fero has downloaded;
/// `None` means nothing has been downloaded yet.
///
/// Licensing does not shortcut this any more — see [`is_licensed`]: a
/// licensed novel gets its real status here (it can be running, finished, or
/// on hiatus) and the license is a separate fact the caller reads on the side.
pub fn resolve(facts: &SeriesStatusFacts, local_last_chapter: Option<u32>) -> SeriesStatus {
    match facts.original {
        OriginalStatus::Hiatus => return SeriesStatus::Hiatus,
        OriginalStatus::Dropped => return SeriesStatus::Dropped,
        OriginalStatus::Unknown => return SeriesStatus::Unknown,
        OriginalStatus::Ongoing => return SeriesStatus::Ongoing,
        OriginalStatus::Completed => {}
    }

    if facts.fully_translated != Some(true) {
        return SeriesStatus::Ongoing;
    }

    match (facts.latest_chapter, local_last_chapter) {
        // Everything the source lists is here.
        (Some(remote), Some(local)) if local >= remote => SeriesStatus::Completed,
        // Chapters are still missing — finished upstream, not finished here.
        (Some(_), _) => SeriesStatus::Ongoing,
        // The source lists no chapter numbers; the two "done" flags have to
        // carry the decision on their own.
        (None, _) => SeriesStatus::Completed,
    }
}

/// Whether the source considers this novel licensed.
///
/// Orthogonal to [`resolve`]'s answer: a licensed novel can be running,
/// finished, or on hiatus, and stays licensed regardless of which. Callers
/// apply this one-way, the same as `completed`/`hiatus`/`dropped` — see
/// [`crate::core::subscription::Subscription::apply_source_status`].
pub fn is_licensed(facts: &SeriesStatusFacts) -> bool {
    facts.licensed == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_english_wordings() {
        assert_eq!(
            classify_status_text("Completed"),
            Some(SeriesStatus::Completed)
        );
        assert_eq!(
            classify_status_text("Complete"),
            Some(SeriesStatus::Completed)
        );
        assert_eq!(
            classify_status_text("Finished"),
            Some(SeriesStatus::Completed)
        );
        assert_eq!(classify_status_text("Ongoing"), Some(SeriesStatus::Ongoing));
        assert_eq!(
            classify_status_text("Publishing"),
            Some(SeriesStatus::Ongoing)
        );
        assert_eq!(classify_status_text("Hiatus"), Some(SeriesStatus::Hiatus));
        assert_eq!(classify_status_text("On Hold"), Some(SeriesStatus::Hiatus));
        assert_eq!(classify_status_text("Dropped"), Some(SeriesStatus::Dropped));
        assert_eq!(
            classify_status_text("Discontinued"),
            Some(SeriesStatus::Dropped)
        );
    }

    #[test]
    fn is_case_and_whitespace_insensitive() {
        assert_eq!(
            classify_status_text("  CoMPLeted  "),
            Some(SeriesStatus::Completed)
        );
    }

    #[test]
    fn an_unrecognized_wording_answers_none_rather_than_guess() {
        assert_eq!(classify_status_text("Season 2 confirmed"), None);
        assert_eq!(classify_status_text(""), None);
        assert_eq!(classify_status_text("   "), None);
    }

    /// A specific word must win over a generic one that happens to sit next
    /// to it — a page can plausibly say both in the same breath.
    #[test]
    fn a_specific_word_outranks_a_generic_one_in_the_same_text() {
        assert_eq!(
            classify_status_text("No longer on hiatus, fully completed"),
            Some(SeriesStatus::Hiatus)
        );
    }

    fn facts(
        original: OriginalStatus,
        translated: Option<bool>,
        licensed: Option<bool>,
        latest: Option<u32>,
    ) -> SeriesStatusFacts {
        SeriesStatusFacts {
            original,
            fully_translated: translated,
            licensed,
            latest_chapter: latest,
        }
    }

    #[test]
    fn all_three_conditions_make_it_complete() {
        let f = facts(
            OriginalStatus::Completed,
            Some(true),
            Some(false),
            Some(142),
        );
        assert_eq!(resolve(&f, Some(142)), SeriesStatus::Completed);
    }

    /// Finished upstream but chapters still missing here: not done. Otherwise
    /// the "complete" edition would be built without its ending.
    #[test]
    fn missing_chapters_prevent_completion() {
        let f = facts(
            OriginalStatus::Completed,
            Some(true),
            Some(false),
            Some(142),
        );
        assert_eq!(resolve(&f, Some(120)), SeriesStatus::Ongoing);
        assert_eq!(resolve(&f, None), SeriesStatus::Ongoing);
    }

    #[test]
    fn untranslated_original_is_not_complete() {
        let f = facts(
            OriginalStatus::Completed,
            Some(false),
            Some(false),
            Some(10),
        );
        assert_eq!(resolve(&f, Some(10)), SeriesStatus::Ongoing);
    }

    /// Licensing used to shortcut `resolve` entirely; now it is a separate
    /// fact that coexists with whatever the real status turns out to be.
    #[test]
    fn licensing_no_longer_shortcuts_the_real_status() {
        let f = facts(OriginalStatus::Completed, Some(true), Some(true), Some(5));
        assert_eq!(resolve(&f, Some(5)), SeriesStatus::Completed);
        assert!(is_licensed(&f));
    }

    #[test]
    fn a_series_can_be_running_and_licensed_at_once() {
        let f = facts(OriginalStatus::Ongoing, None, Some(true), None);
        assert_eq!(resolve(&f, None), SeriesStatus::Ongoing);
        assert!(is_licensed(&f));
    }

    #[test]
    fn no_licensing_signal_answers_false_not_a_guess() {
        let f = facts(OriginalStatus::Ongoing, None, None, None);
        assert!(!is_licensed(&f));
    }

    #[test]
    fn hiatus_and_dropped_pass_through() {
        let h = facts(OriginalStatus::Hiatus, Some(false), Some(false), None);
        assert_eq!(resolve(&h, None), SeriesStatus::Hiatus);
        let d = facts(OriginalStatus::Dropped, Some(false), Some(false), None);
        assert_eq!(resolve(&d, None), SeriesStatus::Dropped);
    }

    #[test]
    fn unknown_stays_unknown() {
        let f = facts(OriginalStatus::Unknown, None, None, None);
        assert_eq!(resolve(&f, Some(50)), SeriesStatus::Unknown);
    }

    /// Without chapter numbers the two flags decide alone — some series list
    /// releases only by name.
    #[test]
    fn without_chapter_numbers_the_flags_decide() {
        let f = facts(OriginalStatus::Completed, Some(true), Some(false), None);
        assert_eq!(resolve(&f, None), SeriesStatus::Completed);
    }

    /// Only a finished serial stops being checked: a dropped one may be picked
    /// up again, and a paused subscription is not "settled", it is idle.
    #[test]
    fn only_completed_stops_the_checks() {
        assert!(SeriesStatus::Completed.is_settled());
        assert!(!SeriesStatus::Dropped.is_settled());
        assert!(!SeriesStatus::Paused.is_settled());
        assert!(!SeriesStatus::Hiatus.is_settled());
    }

    #[test]
    fn a_status_is_due_after_a_week() {
        let now = 10 * STATUS_MAX_AGE_SECS;
        assert!(is_due(None, now), "never checked is always due");
        assert!(is_due(Some(now - STATUS_MAX_AGE_SECS), now));
        assert!(!is_due(Some(now - 60), now));
    }

    /// A clock that jumped backwards must not make everything due at once.
    #[test]
    fn a_timestamp_from_the_future_is_not_due() {
        assert!(!is_due(Some(1_000), 500));
    }

    /// The point of the slow lane: a serial nobody expects anything from is
    /// still looked at, just not on every run.
    #[test]
    fn settled_serials_stay_in_the_slow_lane_but_are_not_dropped() {
        let now = 10 * IDLE_RECHECK_SECS;
        for status in [
            SeriesStatus::Completed,
            SeriesStatus::Dropped,
            SeriesStatus::Hiatus,
        ] {
            assert!(status.checks_rarely(), "{status:?}");
            assert!(
                !should_check(status, false, true, Some(now - 60), now),
                "{status:?}"
            );
            assert!(
                should_check(status, false, true, Some(now - IDLE_RECHECK_SECS), now),
                "{status:?}"
            );
            assert!(should_check(status, false, true, None, now), "{status:?}");
        }
    }

    #[test]
    fn ongoing_and_unknown_are_checked_every_run() {
        let now = 10 * IDLE_RECHECK_SECS;
        for status in [SeriesStatus::Ongoing, SeriesStatus::Unknown] {
            assert!(!status.checks_rarely(), "{status:?}");
            assert!(
                should_check(status, false, true, Some(now - 1), now),
                "{status:?}"
            );
        }
    }

    /// A licensed serial is the one "dead"-looking status that must not slow
    /// down: the chapters disappear with the takedown, whatever the
    /// underlying life-cycle status otherwise says.
    #[test]
    fn a_licensed_serial_is_checked_every_run_even_when_otherwise_settled() {
        let now = 10 * IDLE_RECHECK_SECS;
        assert!(should_check(
            SeriesStatus::Completed,
            true,
            true,
            Some(now - 1),
            now
        ));
    }

    /// Pausing a subscription is the user saying "not now", and that outranks
    /// every status — even a licensed one.
    #[test]
    fn a_disabled_subscription_is_never_checked() {
        assert!(!should_check(SeriesStatus::Ongoing, false, false, None, 1_000));
        assert!(!should_check(SeriesStatus::Ongoing, true, false, None, 1_000));
    }

    /// The whole reason `status_override` exists: a hand setting that the next
    /// check run overwrites is not a setting.
    #[test]
    fn a_hand_setting_outranks_the_source() {
        assert_eq!(
            effective(
                true,
                Some(SeriesStatus::Completed),
                SeriesStatus::Ongoing,
                false,
                false,
                false
            ),
            SeriesStatus::Completed
        );
        assert_eq!(
            effective(
                true,
                Some(SeriesStatus::Ongoing),
                SeriesStatus::Completed,
                true,
                false,
                false
            ),
            SeriesStatus::Ongoing
        );
    }

    #[test]
    fn without_a_hand_setting_the_source_decides_and_the_flags_fill_in() {
        assert_eq!(
            effective(true, None, SeriesStatus::Dropped, true, false, false),
            SeriesStatus::Dropped
        );
        assert_eq!(
            effective(true, None, SeriesStatus::Unknown, true, false, false),
            SeriesStatus::Completed
        );
        assert_eq!(
            effective(true, None, SeriesStatus::Unknown, false, true, false),
            SeriesStatus::Hiatus
        );
        assert_eq!(
            effective(true, None, SeriesStatus::Unknown, false, false, true),
            SeriesStatus::Dropped
        );
        assert_eq!(
            effective(true, None, SeriesStatus::Unknown, false, false, false),
            SeriesStatus::Unknown
        );
    }

    /// A paused *subscription* wins over everything else — even a hand
    /// setting on the series status, because Fero currently checks nothing
    /// here to act on that setting anyway.
    #[test]
    fn a_paused_subscription_outranks_even_a_hand_setting() {
        assert_eq!(
            effective(
                false,
                Some(SeriesStatus::Completed),
                SeriesStatus::Ongoing,
                false,
                false,
                false
            ),
            SeriesStatus::Paused
        );
    }

    #[test]
    fn every_status_survives_the_round_trip_through_the_api() {
        for status in [
            SeriesStatus::Ongoing,
            SeriesStatus::Completed,
            SeriesStatus::Hiatus,
            SeriesStatus::Dropped,
            SeriesStatus::Paused,
            SeriesStatus::Unknown,
        ] {
            assert_eq!(SeriesStatus::from_id(status.as_id()), Some(status));
        }
        assert_eq!(SeriesStatus::from_id("erledigt"), None);
    }

    /// Old files (before 09/2026) wrote the English wire values, and one
    /// wrote "licensed" as its own status — both must still load, or an
    /// upgrade makes existing subscriptions silently disappear from the list.
    #[test]
    fn legacy_english_wire_values_still_deserialize() {
        for (legacy, expected) in [
            ("\"ongoing\"", SeriesStatus::Ongoing),
            ("\"completed\"", SeriesStatus::Completed),
            ("\"hiatus\"", SeriesStatus::Hiatus),
            ("\"dropped\"", SeriesStatus::Dropped),
            ("\"unknown\"", SeriesStatus::Unknown),
            ("\"licensed\"", SeriesStatus::Ongoing),
        ] {
            assert_eq!(
                serde_json::from_str::<SeriesStatus>(legacy).expect(legacy),
                expected,
                "{legacy}"
            );
        }
    }

    #[test]
    fn fresh_writes_use_the_german_wire_values() {
        for (status, expected) in [
            (SeriesStatus::Ongoing, "\"laufend\""),
            (SeriesStatus::Completed, "\"abgeschlossen\""),
            (SeriesStatus::Hiatus, "\"hiatus\""),
            (SeriesStatus::Dropped, "\"abgebrochen\""),
            (SeriesStatus::Paused, "\"pausiert\""),
            (SeriesStatus::Unknown, "\"unbekannt\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }

    fn comic(publication: OriginalStatus, source_completed: Option<bool>) -> ComicStatusFacts {
        ComicStatusFacts {
            publication,
            source_completed,
        }
    }

    /// The case from the bug report: the database says finished, the archive
    /// is complete — that is "abgeschlossen".
    #[test]
    fn a_finished_comic_with_nothing_pending_is_complete() {
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Completed, Some(true)), 0),
            SeriesStatus::Completed
        );
        // The scanlation site alone is enough; not every series is in AniList.
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Unknown, Some(true)), 0),
            SeriesStatus::Completed
        );
    }

    /// Finished upstream but chapters still missing here: the slow lane would
    /// stop fetching the very chapters that are left.
    #[test]
    fn pending_chapters_keep_a_finished_comic_in_the_fast_lane() {
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Completed, Some(true)), 12),
            SeriesStatus::Ongoing
        );
    }

    /// A site that labels a stalled series "Completed" means "we stopped".
    /// The database knows better, and dropped is not finished.
    #[test]
    fn the_database_outranks_the_site_on_dropped_and_hiatus() {
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Dropped, Some(true)), 0),
            SeriesStatus::Dropped
        );
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Hiatus, Some(true)), 0),
            SeriesStatus::Hiatus
        );
    }

    /// Nothing known stays nothing known — never a guessed "finished".
    #[test]
    fn a_comic_nobody_has_an_opinion_on_stays_unknown() {
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Unknown, None), 0),
            SeriesStatus::Unknown
        );
        assert_eq!(
            resolve_comic(&comic(OriginalStatus::Ongoing, Some(false)), 0),
            SeriesStatus::Ongoing
        );
    }

    #[test]
    fn attention_is_for_the_two_that_change_plans() {
        assert!(SeriesStatus::Dropped.needs_attention());
        assert!(SeriesStatus::Hiatus.needs_attention());
        assert!(!SeriesStatus::Ongoing.needs_attention());
        assert!(!SeriesStatus::Completed.needs_attention());
        assert!(!SeriesStatus::Paused.needs_attention());
        // Licensed is a separate fact now — callers OR it in themselves; see
        // the webnovel/manga summary construction.
    }
}
