//! Deciding what a serial's status *means*.
//!
//! The reading of a NovelUpdates page happens in `api::novel::status`; this
//! module turns those raw facts into the one decision that changes behaviour:
//! is the serial finished, so the complete edition can be built and periodic
//! checks can stop?
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
        let f = facts(OriginalStatus::Completed, Some(true), Some(false), Some(142));
        assert_eq!(resolve(&f, Some(142)), SeriesStatus::Completed);
    }

    /// Finished upstream but chapters still missing here: not done. Otherwise
    /// the "complete" edition would be built without its ending.
    #[test]
    fn missing_chapters_prevent_completion() {
        let f = facts(OriginalStatus::Completed, Some(true), Some(false), Some(142));
        assert_eq!(resolve(&f, Some(120)), SeriesStatus::Ongoing);
        assert_eq!(resolve(&f, None), SeriesStatus::Ongoing);
    }

    #[test]
    fn untranslated_original_is_not_complete() {
        let f = facts(OriginalStatus::Completed, Some(false), Some(false), Some(10));
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
    fn attention_is_for_the_three_that_change_plans() {
        assert!(SeriesStatus::Licensed.needs_attention());
        assert!(SeriesStatus::Dropped.needs_attention());
        assert!(SeriesStatus::Hiatus.needs_attention());
        assert!(!SeriesStatus::Ongoing.needs_attention());
        assert!(!SeriesStatus::Completed.needs_attention());
    }
}
