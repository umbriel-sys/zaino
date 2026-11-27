//! What a catch-up build tells the operator: that it started, how far it is
//! and how fast, and that it finished. A build of a few blocks, as each new
//! block at the tip is, says nothing.

use std::time::{Duration, Instant};

use zaino_indexes::IndexId;
use zaino_primitives::types::Height;

/// Blocks a build must cover to be reported.
const CATCH_UP_BLOCKS: u32 = 100;

/// How often a catch-up build reports its progress.
const REPORT_EVERY: Duration = Duration::from_secs(30);

/// Blocks from `from` (none = an empty store) through `target`.
fn blocks_to_go(from: Option<Height>, target: Height) -> u32 {
    let target = u32::from(target);
    match from {
        Some(from) => target.saturating_sub(u32::from(from)),
        None => target.saturating_add(1),
    }
}

/// Blocks per second over `over`, and the time `to_go` blocks take at that
/// rate; no estimate while nothing has advanced.
fn pace(advanced: u32, over: Duration, to_go: u32) -> (u64, Option<Duration>) {
    let millis = u64::try_from(over.as_millis()).unwrap_or(u64::MAX).max(1);
    let per_sec = u64::from(advanced)
        .saturating_mul(1_000)
        .checked_div(millis)
        .unwrap_or(0);
    let remaining = u64::from(to_go)
        .saturating_mul(millis)
        .checked_div(u64::from(advanced))
        .map(Duration::from_millis);
    (per_sec, remaining)
}

/// `duration` to its two largest units: `2h14m`, `33m05s`, `45s`.
fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (hours, minutes, seconds) = (secs / 3_600, secs % 3_600 / 60, secs % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds:02}s"),
        _ => format!("{hours}h{minutes:02}m"),
    }
}

/// `part` of `whole` to a tenth of a percent.
fn percent(part: u32, whole: u32) -> String {
    let per_mille = u64::from(part)
        .saturating_mul(1_000)
        .checked_div(u64::from(whole))
        .unwrap_or(0);
    format!("{}.{}%", per_mille / 10, per_mille % 10)
}

/// One catch-up build's reporting.
pub(crate) struct Report {
    started: Instant,
    from: Option<Height>,
    target: Height,
    last: (Instant, Option<Height>),
}

impl Report {
    /// Announces a build from `from` to `target` over `indexes`; `None` (and
    /// silence) for one too short to report.
    pub(crate) fn start(
        from: Option<Height>,
        target: Height,
        indexes: impl Iterator<Item = IndexId>,
    ) -> Option<Self> {
        let blocks = blocks_to_go(from, target);
        if blocks <= CATCH_UP_BLOCKS {
            return None;
        }
        tracing::info!(
            from = ?from.map(u32::from),
            to = u32::from(target),
            blocks,
            indexes = ?indexes.map(IndexId::name).collect::<Vec<_>>(),
            "building the store to the chain head's floor: the validator answers what it does not hold yet"
        );
        let now = Instant::now();
        Some(Self {
            started: now,
            from,
            target,
            last: (now, from),
        })
    }

    /// Reports progress at `height` (the lowest index's, `slowest`), once
    /// every [`REPORT_EVERY`].
    pub(crate) fn tick(&mut self, height: Option<Height>, slowest: Option<IndexId>) {
        let (last_at, last_height) = self.last;
        let over = last_at.elapsed();
        if over < REPORT_EVERY {
            return;
        }
        let advanced = blocks_to_go(last_height, self.target)
            .saturating_sub(blocks_to_go(height, self.target));
        let to_go = blocks_to_go(height, self.target);
        let total = blocks_to_go(self.from, self.target);
        let (blocks_per_sec, remaining) = pace(advanced, over, to_go);
        tracing::info!(
            height = ?height.map(u32::from),
            target = u32::from(self.target),
            done = %percent(total.saturating_sub(to_go), total),
            blocks_per_sec,
            remaining = %remaining.map_or_else(|| "unknown".to_owned(), human),
            slowest_index = ?slowest.map(IndexId::name),
            "store build progress"
        );
        self.last = (Instant::now(), height);
    }

    /// Reports the build's end at `height`: built, or stopped with `error`.
    pub(crate) fn finish(self, height: Option<Height>, error: Option<&dyn std::fmt::Display>) {
        let took = human(self.started.elapsed());
        match error {
            None => tracing::info!(
                height = ?height.map(u32::from),
                %took,
                "the store is built to the chain head's floor"
            ),
            Some(error) => tracing::warn!(
                height = ?height.map(u32::from),
                target = u32::from(self.target),
                %took,
                %error,
                "the store build stopped before its target: retried at the next chain head update"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("in range")
    }

    #[test]
    fn a_build_of_a_few_blocks_is_not_reported() {
        let none = std::iter::empty;
        assert!(Report::start(Some(height(1_000)), height(1_001), none()).is_none());
        assert!(Report::start(Some(height(1_000)), height(1_100), none()).is_none());
        assert!(Report::start(Some(height(1_000)), height(1_101), none()).is_some());
        // an empty store has the genesis block to build too
        assert!(Report::start(None, height(99), none()).is_none());
        assert!(Report::start(None, height(100), none()).is_some());
    }

    #[test]
    fn the_pace_is_blocks_per_second_and_the_time_left_at_that_rate() {
        let (per_sec, remaining) = pace(54_000, Duration::from_secs(30), 3_545_097);
        assert_eq!(per_sec, 1_800);
        assert_eq!(remaining.map(human).as_deref(), Some("32m49s"));
        // nothing advanced: no rate to estimate from
        assert_eq!(pace(0, Duration::from_secs(30), 10), (0, None));
        // a zero interval does not divide by zero
        assert_eq!(pace(5, Duration::ZERO, 0).0, 5_000);
    }

    #[test]
    fn durations_and_percentages_read_at_a_glance() {
        assert_eq!(human(Duration::from_secs(45)), "45s");
        assert_eq!(human(Duration::from_secs(33 * 60 + 5)), "33m05s");
        assert_eq!(
            human(Duration::from_secs(2 * 3_600 + 14 * 60 + 59)),
            "2h14m"
        );
        assert_eq!(percent(54_000, 3_599_098), "1.5%");
        assert_eq!(percent(0, 0), "0.0%");
        assert_eq!(percent(10, 10), "100.0%");
    }
}
