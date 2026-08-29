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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeriesStatus {
    /// Still receiving chapters.
    Ongoing,
    /// Finished: original done, translation done, and everything is here.
    Completed,
    /// Paused upstream.
    Hiatus,
    /// Abandoned upstream.
    Dropped,
    /// Licensed — the translation is likely to disappear.
    Licensed,
    /// Not determined yet.
    #[default]
    Unknown,
}

impl SeriesStatus {
    /// Whether this status warrants telling the user about it unprompted.
    ///
    /// The three that change what someone would do: a licensed serial should be
    /// downloaded *now*, a dropped one will never finish, a paused one is not
    /// broken but idle.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Licensed | Self::Dropped | Self::Hiatus)
    }

    /// Whether periodic checks can be skipped.
    ///
    /// Only a finished serial qualifies. A dropped one might still get picked
    /// up by another translator, and a licensed one may keep releasing until
    /// the takedown — stopping the checks would miss exactly the chapters that
    /// matter most.
    pub fn is_settled(self) -> bool {
        self == Self::Completed
    }

    /// Whether this status puts a serial in the slow lane — still checked,
    /// just rarely.
    ///
    /// The three statuses that mean "nothing is coming, probably". *Probably*
    /// is why they are checked at all: translators pick a dropped series up
    /// years later, a hiatus ends, and a finished work grows a sequel arc in
    /// the same entry. `Licensed` deliberately stays in the fast lane — it is
    /// the one status where a missed run costs chapters that never come back.
    pub fn checks_rarely(self) -> bool {
        matches!(self, Self::Completed | Self::Dropped | Self::Hiatus)
    }

    /// The wire name, matching the `serde` representation.
    ///
    /// Needed because the status travels through the API as a string the user
    /// picks in a dropdown, and it has to come back in.
    pub fn as_id(self) -> &'static str {
        match self {
            Self::Ongoing => "ongoing",
            Self::Completed => "completed",
            Self::Hiatus => "hiatus",
            Self::Dropped => "dropped",
            Self::Licensed => "licensed",
            Self::Unknown => "unknown",
        }
    }

    /// Parses a wire name back into a status; `None` for anything else.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "ongoing" => Some(Self::Ongoing),
            "completed" => Some(Self::Completed),
            "hiatus" => Some(Self::Hiatus),
            "dropped" => Some(Self::Dropped),
            "licensed" => Some(Self::Licensed),
            "unknown" => Some(Self::Unknown),
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
/// A single subscription checked by hand does not come through here: an
/// explicit click always runs, whatever the status says.
pub fn should_check(
    status: SeriesStatus,
    enabled: bool,
    last_check: Option<u64>,
    now: u64,
) -> bool {
    if !enabled {
        return false;
    }
    if !status.checks_rarely() {
        return true;
    }
    match last_check {
        None => true,
        Some(then) => now.saturating_sub(then) >= IDLE_RECHECK_SECS,
    }
}

/// The status that actually applies, out of the three that can disagree.
///
/// Precedence, strongest first: what the user set by hand, what the status
/// source last said, and finally the `completed`/`hiatus` flags. The hand
/// setting has to win outright — otherwise the next check run silently undoes
/// it, which is what happened while `completed` served as both the user's
/// switch and the scraper's output.
pub fn effective(
    manual: Option<SeriesStatus>,
    detected: SeriesStatus,
    completed: bool,
    hiatus: bool,
) -> SeriesStatus {
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
    } else {
        SeriesStatus::Unknown
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
pub fn resolve(facts: &SeriesStatusFacts, local_last_chapter: Option<u32>) -> SeriesStatus {
    // Licensing outranks everything: it is the only status that says "act now".
    if facts.licensed == Some(true) {
        return SeriesStatus::Licensed;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Licensing is the one fact that outranks the rest — it is the only status
    /// that means "download it now, it may be gone next week".
    #[test]
    fn licensing_wins_over_everything() {
        let f = facts(OriginalStatus::Completed, Some(true), Some(true), Some(5));
        assert_eq!(resolve(&f, Some(5)), SeriesStatus::Licensed);
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
    /// up again, a licensed one keeps releasing until the takedown.
    #[test]
    fn only_completed_stops_the_checks() {
        assert!(SeriesStatus::Completed.is_settled());
        assert!(!SeriesStatus::Dropped.is_settled());
        assert!(!SeriesStatus::Licensed.is_settled());
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
                !should_check(status, true, Some(now - 60), now),
                "{status:?}"
            );
            assert!(
                should_check(status, true, Some(now - IDLE_RECHECK_SECS), now),
                "{status:?}"
            );
            assert!(should_check(status, true, None, now), "{status:?}");
        }
    }

    /// A licensed serial is the one "dead" status that must not slow down:
    /// the chapters disappear with the takedown.
    #[test]
    fn living_and_licensed_serials_are_checked_every_run() {
        let now = 10 * IDLE_RECHECK_SECS;
        for status in [
            SeriesStatus::Ongoing,
            SeriesStatus::Unknown,
            SeriesStatus::Licensed,
        ] {
            assert!(!status.checks_rarely(), "{status:?}");
            assert!(should_check(status, true, Some(now - 1), now), "{status:?}");
        }
    }

    /// Pausing a subscription is the user saying "not now", and that outranks
    /// every status.
    #[test]
    fn a_disabled_subscription_is_never_checked() {
        assert!(!should_check(SeriesStatus::Ongoing, false, None, 1_000));
    }

    /// The whole reason `status_override` exists: a hand setting that the next
    /// check run overwrites is not a setting.
    #[test]
    fn a_hand_setting_outranks_the_source() {
        assert_eq!(
            effective(
                Some(SeriesStatus::Completed),
                SeriesStatus::Ongoing,
                false,
                false
            ),
            SeriesStatus::Completed
        );
        assert_eq!(
            effective(
                Some(SeriesStatus::Ongoing),
                SeriesStatus::Completed,
                true,
                false
            ),
            SeriesStatus::Ongoing
        );
    }

    #[test]
    fn without_a_hand_setting_the_source_decides_and_the_flags_fill_in() {
        assert_eq!(
            effective(None, SeriesStatus::Dropped, true, false),
            SeriesStatus::Dropped
        );
        assert_eq!(
            effective(None, SeriesStatus::Unknown, true, false),
            SeriesStatus::Completed
        );
        assert_eq!(
            effective(None, SeriesStatus::Unknown, false, true),
            SeriesStatus::Hiatus
        );
        assert_eq!(
            effective(None, SeriesStatus::Unknown, false, false),
            SeriesStatus::Unknown
        );
    }

    #[test]
    fn every_status_survives_the_round_trip_through_the_api() {
        for status in [
            SeriesStatus::Ongoing,
            SeriesStatus::Completed,
            SeriesStatus::Hiatus,
            SeriesStatus::Dropped,
            SeriesStatus::Licensed,
            SeriesStatus::Unknown,
        ] {
            assert_eq!(SeriesStatus::from_id(status.as_id()), Some(status));
        }
        assert_eq!(SeriesStatus::from_id("erledigt"), None);
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
    fn attention_is_for_the_three_that_change_plans() {
        assert!(SeriesStatus::Licensed.needs_attention());
        assert!(SeriesStatus::Dropped.needs_attention());
        assert!(SeriesStatus::Hiatus.needs_attention());
        assert!(!SeriesStatus::Ongoing.needs_attention());
        assert!(!SeriesStatus::Completed.needs_attention());
    }
}
