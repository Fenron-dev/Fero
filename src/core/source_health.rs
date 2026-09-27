//! # core::source_health
//!
//! Tells a broken *site* apart from a broken *title*.
//!
//! A single subscription failing is usually about the work, not the source:
//! pulled down, moved, or a page whose layout briefly hiccuped. A whole
//! *site* being unreachable — a Cloudflare change, DNS trouble, a redesign
//! the adapter no longer understands — instead shows up as every enabled
//! subscription on that host failing at the same time. That is the line this
//! module draws: a host is flagged only when *all* of its enabled
//! subscriptions are currently failing, and have been for a while — one
//! flaky title alone never lights up the warning, and a genuine outage does
//! within [`UNREACHABLE_MIN_AGE_SECS`].
//!
//! Grouping is by host, not by adapter id: `generic` covers dozens of
//! unrelated sites, and `madara`/`themesia` are shared engines behind many
//! independent ones — an adapter id says nothing about which *site* is down.
//!
//! ## Dependencies
//! - none — pure aggregation, so it is testable without a workspace on disk

use serde::Serialize;
use std::collections::BTreeMap;

/// A host down for less than this is not worth an icon yet — long enough
/// that a retried transient blip has had time to resolve on its own.
pub const UNREACHABLE_MIN_AGE_SECS: u64 = 24 * 60 * 60;

/// One subscription's reachability, reduced to what this module needs.
#[derive(Debug, Clone)]
pub struct HostSample {
    /// Host the subscription's overview page lives on.
    pub host: String,
    /// Paused subscriptions carry no signal — the user asked Fero to leave
    /// them alone, so a stale failure on one must not flag the whole host.
    pub enabled: bool,
    /// The last check's error, or `None` if it succeeded.
    pub last_error: Option<String>,
    /// The last time this subscription's fetch actually succeeded.
    ///
    /// Callers fall back to the subscription's creation time when it never
    /// has — a lower bound on how long a current failure has lasted, not a
    /// claim that it started exactly then.
    pub last_seen_reachable_unix: u64,
}

/// A host all of whose enabled subscriptions are currently failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreachableHost {
    pub host: String,
    /// How many enabled subscriptions are on this host (all of them failing).
    pub affected: usize,
    /// UNIX timestamp since which *every* one of them has been failing —
    /// the most recent "last reachable" among them, since before that point
    /// at least one still worked.
    pub unreachable_since_unix: u64,
    /// A representative error message, for a tooltip. Not guaranteed to be
    /// the same wording every subscription on the host is failing with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Groups `samples` by host and returns every host whose enabled
/// subscriptions are, without exception, currently failing — and have been
/// for at least [`UNREACHABLE_MIN_AGE_SECS`].
pub fn unreachable_hosts(samples: &[HostSample], now: u64) -> Vec<UnreachableHost> {
    let mut by_host: BTreeMap<&str, Vec<&HostSample>> = BTreeMap::new();
    for sample in samples.iter().filter(|sample| sample.enabled) {
        by_host.entry(sample.host.as_str()).or_default().push(sample);
    }

    let mut hosts = Vec::new();
    for (host, entries) in by_host {
        // `entries` always has at least one sample — it only exists as a key
        // because something was pushed into it above.
        let all_failing = entries.iter().all(|sample| sample.last_error.is_some());
        if !all_failing {
            continue;
        }
        let affected = entries.len();
        // Not reachable since the latest of "when each one last worked" —
        // before that instant, at least one of them still did.
        let unreachable_since_unix = entries
            .iter()
            .map(|sample| sample.last_seen_reachable_unix)
            .max()
            .unwrap_or(now);
        if now.saturating_sub(unreachable_since_unix) < UNREACHABLE_MIN_AGE_SECS {
            continue;
        }
        let last_error = entries
            .iter()
            .find_map(|sample| sample.last_error.clone());
        hosts.push(UnreachableHost {
            host: host.to_string(),
            affected,
            unreachable_since_unix,
            last_error,
        });
    }
    hosts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    fn sample(
        host: &str,
        enabled: bool,
        failing: bool,
        last_seen_reachable_unix: u64,
    ) -> HostSample {
        HostSample {
            host: host.to_string(),
            enabled,
            last_error: failing.then(|| "Zeitüberschreitung".to_string()),
            last_seen_reachable_unix,
        }
    }

    #[test]
    fn one_flaky_title_does_not_flag_a_healthy_host() {
        let now = 10 * DAY;
        let samples = vec![
            sample("example.com", true, true, now - 2 * DAY),
            sample("example.com", true, false, now - 60),
        ];

        assert_eq!(unreachable_hosts(&samples, now), Vec::new());
    }

    #[test]
    fn a_host_down_for_everyone_and_long_enough_is_flagged() {
        let now = 10 * DAY;
        let samples = vec![
            sample("example.com", true, true, now - 3 * DAY),
            sample("example.com", true, true, now - 2 * DAY),
        ];

        let hosts = unreachable_hosts(&samples, now);

        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "example.com");
        assert_eq!(hosts[0].affected, 2);
        // The later of the two — the first one already still worked then.
        assert_eq!(hosts[0].unreachable_since_unix, now - 2 * DAY);
        assert!(hosts[0].last_error.is_some());
    }

    #[test]
    fn a_fresh_failure_does_not_flag_before_the_minimum_age() {
        let now = 10 * DAY;
        let samples = vec![sample("example.com", true, true, now - 60)];

        assert_eq!(unreachable_hosts(&samples, now), Vec::new());
    }

    #[test]
    fn a_paused_subscription_carries_no_signal() {
        let now = 40 * DAY;
        let samples = vec![sample("example.com", false, true, now - 30 * DAY)];

        assert_eq!(unreachable_hosts(&samples, now), Vec::new());
    }

    #[test]
    fn different_hosts_are_judged_independently() {
        let now = 10 * DAY;
        let samples = vec![
            sample("broken.example", true, true, now - 5 * DAY),
            sample("fine.example", true, false, now - 60),
        ];

        let hosts = unreachable_hosts(&samples, now);

        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "broken.example");
    }
}
