//! The height bracket of a block-time range, for `getblockhashes` (lineage
//! B's `CandidateSearch`).
//!
//! Block times are not monotonic, so a time range is not a slice of heights.
//! What is monotonic is the median time past (MTP), and consensus bounds a
//! block's time by it: `MTP < nTime <= MTP + drift` where the drift rule is
//! enforced. The search binary-searches MTP for the smallest bracket that
//! must hold every block with `low <= nTime < high`; the caller then filters
//! the bracket by each block's own time. It is a state machine that asks for
//! block times rather than fetching them, so it holds no I/O.

use zaino_consensus::{median_time_past, BLOCK_MAX_TIME_SINCE_MEDIAN, MEDIAN_TIME_SPAN};
use zaino_primitives::types::{BlockTime, Height, Network};

/// Where the drift rule `nTime <= MTP + drift` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Drift {
    /// The first height it is enforced at.
    enforced_from: u32,
}

impl Drift {
    /// Mainnet enforces it everywhere; a testnet (Regtest included, as zebra
    /// treats it) from `TESTNET_MAX_TIME_START_HEIGHT`.
    pub(crate) fn of(network: Network) -> Self {
        Self {
            enforced_from: match network {
                Network::Mainnet => 0,
                Network::PubTestnet | Network::Regtest => {
                    zaino_consensus::TESTNET_MAX_TIME_START_HEIGHT
                }
            },
        }
    }

    #[cfg(test)]
    const fn activating_at(enforced_from: u32) -> Self {
        Self { enforced_from }
    }
}

/// An inclusive height range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bracket {
    pub(crate) start: Height,
    pub(crate) end: Height,
}

/// A header the search needed is missing at or below the tip: a hole in the
/// caller's view, never an absent block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MissingHeader {
    pub(crate) height: Height,
}

/// What the search needs next, or its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchStep {
    /// The block time at this height.
    Need(Height),
    /// The bracket, `None` when no block can match.
    Done(Option<Bracket>),
    /// A header at or below the tip was missing.
    Failed(MissingHeader),
}

/// The search for the bracket of `[low, high)` below a tip.
#[derive(Clone, Debug)]
pub(crate) struct CandidateSearch {
    tip: Height,
    low: BlockTime,
    high: BlockTime,
    drift: Drift,
    cache: Vec<(Height, Option<BlockTime>)>,
    phase: Phase,
}

/// A binary search over an inclusive interval of heights.
#[derive(Clone, Copy, Debug)]
struct BinarySearch {
    lo: u32,
    hi: u32,
    mid: u32,
    best: Option<u32>,
}

impl BinarySearch {
    fn new(lo: u32, hi: u32) -> Self {
        Self {
            lo,
            hi,
            mid: midpoint(lo, hi),
            best: None,
        }
    }

    fn finished(&self) -> bool {
        self.lo > self.hi
    }

    fn recompute_mid(&mut self) {
        if !self.finished() {
            self.mid = midpoint(self.lo, self.hi);
        }
    }

    /// The largest height where the predicate holds (true low, false high).
    fn record_take_highest(&mut self, holds: bool) {
        if holds {
            self.best = Some(self.mid);
            self.lo = self.mid.saturating_add(1);
        } else {
            self.hi = self.mid.saturating_sub(1);
            if self.mid == 0 {
                self.lo = self.hi.saturating_add(1);
            }
        }
        self.recompute_mid();
    }

    /// The smallest height where the predicate holds (false low, true high).
    fn record_take_lowest(&mut self, holds: bool) {
        if holds {
            self.best = Some(self.mid);
            self.hi = self.mid.saturating_sub(1);
            if self.mid == 0 {
                self.lo = self.hi.saturating_add(1);
            }
        } else {
            self.lo = self.mid.saturating_add(1);
        }
        self.recompute_mid();
    }
}

fn midpoint(lo: u32, hi: u32) -> u32 {
    lo.saturating_add(hi.saturating_sub(lo).checked_div(2).unwrap_or(0))
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    /// The chain is genesis alone.
    GenesisOnly,
    /// The bracket's top: the largest height with `MTP < high`.
    FindHigh(BinarySearch),
    /// The bracket's bottom: the smallest with `MTP >= low - drift`.
    FindLow {
        search: BinarySearch,
        high_bound: Height,
    },
    Done(Option<Bracket>),
    Failed(MissingHeader),
}

enum MtpAttempt {
    NotYetSupplied(Height),
    Hole(Height),
    Ready(BlockTime),
}

impl CandidateSearch {
    pub(crate) fn new(tip: Height, low: BlockTime, high: BlockTime, drift: Drift) -> Self {
        let phase = if low >= high {
            Phase::Done(None)
        } else if tip == Height::GENESIS {
            Phase::GenesisOnly
        } else {
            Phase::FindHigh(BinarySearch::new(1, u32::from(tip)))
        };
        Self {
            tip,
            low,
            high,
            drift,
            cache: Vec::new(),
            phase,
        }
    }

    /// The block time at `height`, `None` if no block is there.
    pub(crate) fn supply(&mut self, height: Height, time: Option<BlockTime>) {
        match self.cache.binary_search_by_key(&height, |(h, _)| *h) {
            Ok(index) => {
                if let Some(entry) = self.cache.get_mut(index) {
                    entry.1 = time;
                }
            }
            Err(index) => self.cache.insert(index, (height, time)),
        }
    }

    fn cached(&self, height: Height) -> Option<Option<BlockTime>> {
        self.cache
            .binary_search_by_key(&height, |(h, _)| *h)
            .ok()
            .and_then(|index| self.cache.get(index))
            .map(|(_, time)| *time)
    }

    /// `MTP(target)` from the cache, for `target >= 1`.
    fn mtp_of(&self, target: u32) -> MtpAttempt {
        let span = u32::try_from(MEDIAN_TIME_SPAN).unwrap_or(u32::MAX);
        let first = target.saturating_sub(span);
        let last = target.saturating_sub(1);
        let mut times: Vec<BlockTime> = Vec::new();
        let mut last_valid: Option<Height> = None;
        for predecessor in first..=last {
            let Ok(height) = Height::try_from(predecessor) else {
                continue;
            };
            last_valid = Some(height);
            match self.cached(height) {
                None => return MtpAttempt::NotYetSupplied(height),
                // at or below the tip a block must exist: a hole, never a
                // sample to skip (it would lower the MTP)
                Some(None) => return MtpAttempt::Hole(height),
                Some(Some(time)) => times.push(time),
            }
        }
        match median_time_past(&times) {
            Some(mtp) => MtpAttempt::Ready(mtp),
            None => MtpAttempt::Hole(last_valid.unwrap_or(Height::GENESIS)),
        }
    }

    /// Advances as far as the cache allows.
    pub(crate) fn poll(&mut self) -> SearchStep {
        loop {
            match self.phase {
                Phase::Done(bracket) => return SearchStep::Done(bracket),
                Phase::Failed(missing) => return SearchStep::Failed(missing),
                Phase::GenesisOnly => match self.cached(Height::GENESIS) {
                    None => return SearchStep::Need(Height::GENESIS),
                    Some(None) => {
                        self.phase = Phase::Failed(MissingHeader {
                            height: Height::GENESIS,
                        });
                    }
                    Some(Some(time)) => {
                        let in_range = self.low <= time && time < self.high;
                        self.phase = Phase::Done(in_range.then_some(Bracket {
                            start: Height::GENESIS,
                            end: Height::GENESIS,
                        }));
                    }
                },
                Phase::FindHigh(mut search) => {
                    if search.finished() {
                        self.start_low_phase(search.best);
                    } else {
                        match self.mtp_of(search.mid) {
                            MtpAttempt::NotYetSupplied(height) => return SearchStep::Need(height),
                            MtpAttempt::Hole(height) => {
                                self.phase = Phase::Failed(MissingHeader { height });
                            }
                            MtpAttempt::Ready(mtp) => {
                                search.record_take_highest(mtp < self.high);
                                self.phase = Phase::FindHigh(search);
                            }
                        }
                    }
                }
                Phase::FindLow {
                    mut search,
                    high_bound,
                } => {
                    if search.finished() {
                        self.finish_low_phase(search.best, high_bound);
                    } else {
                        match self.mtp_of(search.mid) {
                            MtpAttempt::NotYetSupplied(height) => return SearchStep::Need(height),
                            MtpAttempt::Hole(height) => {
                                self.phase = Phase::Failed(MissingHeader { height });
                            }
                            MtpAttempt::Ready(mtp) => {
                                let threshold =
                                    self.low.saturating_sub(BLOCK_MAX_TIME_SINCE_MEDIAN);
                                search.record_take_lowest(mtp >= threshold);
                                self.phase = Phase::FindLow { search, high_bound };
                            }
                        }
                    }
                }
            }
        }
    }

    fn start_low_phase(&mut self, high_best: Option<u32>) {
        let Some(high) = high_best else {
            // even MTP(1), the genesis time, is >= high: nothing matches
            self.phase = Phase::Done(None);
            return;
        };
        let high_bound = Height::try_from(high).unwrap_or(self.tip);
        if self.drift.enforced_from > 0 {
            // blocks before the rule are unbounded above their MTP: none can
            // be excluded from below, so scan to genesis
            self.finalize(Height::GENESIS, high_bound);
            return;
        }
        self.phase = Phase::FindLow {
            search: BinarySearch::new(1, high),
            high_bound,
        };
    }

    fn finish_low_phase(&mut self, low_best: Option<u32>, high_bound: Height) {
        let Some(low) = low_best else {
            // every block up to the top has nTime <= MTP + drift < low
            self.phase = Phase::Done(None);
            return;
        };
        // MTP(1) is the genesis time: genesis stays unless MTP(1) excludes it
        let low_bound = if low <= 1 {
            Height::GENESIS
        } else {
            Height::try_from(low).unwrap_or(Height::GENESIS)
        };
        self.finalize(low_bound, high_bound);
    }

    fn finalize(&mut self, start: Height, end: Height) {
        self.phase = Phase::Done((start <= end).then_some(Bracket { start, end }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GENESIS_TIME: u32 = 1_477_000_000;
    const MAINNET: Drift = Drift::activating_at(0);

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height in range")
    }

    /// Drives a search over a chain's times, by height.
    fn run(tip: u32, low: u32, high: u32, drift: Drift, times: &[u32]) -> Option<Bracket> {
        let mut search = CandidateSearch::new(height(tip), low, high, drift);
        loop {
            match search.poll() {
                SearchStep::Need(h) => {
                    let index = usize::try_from(u32::from(h)).expect("fits");
                    search.supply(h, times.get(index).copied());
                }
                SearchStep::Done(bracket) => return bracket,
                SearchStep::Failed(missing) => panic!("a hole in a whole chain: {missing:?}"),
            }
        }
    }

    /// Drives a search answering `None` at `hole` only.
    fn run_with_hole(mut search: CandidateSearch, times: &[u32], hole: u32) -> SearchStep {
        loop {
            match search.poll() {
                SearchStep::Need(h) => {
                    let requested = u32::from(h);
                    let time = (requested != hole)
                        .then(|| {
                            times
                                .get(usize::try_from(requested).expect("fits"))
                                .copied()
                        })
                        .flatten();
                    search.supply(h, time);
                }
                terminal => return terminal,
            }
        }
    }

    /// Every height whose time is in `[low, high)`.
    fn brute_force(low: u32, high: u32, times: &[u32]) -> Vec<u32> {
        times
            .iter()
            .enumerate()
            .filter(|(_, &time)| low <= time && time < high)
            .map(|(index, _)| u32::try_from(index).expect("fits"))
            .collect()
    }

    /// The bracket filtered by each block's time, as the caller serves it.
    fn bracket_filtered(bracket: Option<Bracket>, low: u32, high: u32, times: &[u32]) -> Vec<u32> {
        let Some(bracket) = bracket else {
            return Vec::new();
        };
        (u32::from(bracket.start)..=u32::from(bracket.end))
            .filter(|&h| {
                times
                    .get(usize::try_from(h).expect("fits"))
                    .is_some_and(|&time| low <= time && time < high)
            })
            .collect()
    }

    /// A consensus-valid chain: each block's time is its MTP plus a delta in
    /// `[1, drift]`, so large deltas then small ones go backwards in time.
    fn chain_from_deltas(deltas: &[u32]) -> Vec<u32> {
        let mut times = vec![GENESIS_TIME];
        for &delta in deltas {
            let mtp = median_time_past(&times).expect("non-empty");
            times.push(mtp + delta.clamp(1, BLOCK_MAX_TIME_SINCE_MEDIAN));
        }
        times
    }

    #[test]
    fn inverted_and_empty_ranges_are_empty() {
        let times = chain_from_deltas(&[60; 20]);
        assert_eq!(run(20, 500, 100, MAINNET, &times), None);
        assert_eq!(run(20, times[5], times[5], MAINNET, &times), None);
        assert_eq!(run(20, 0, GENESIS_TIME, MAINNET, &times), None);
        let after = times[20] + 10_000;
        assert_eq!(run(20, after, after + 10_000, MAINNET, &times), None);
    }

    #[test]
    fn a_genesis_only_chain_is_decided_directly() {
        let times = [GENESIS_TIME];
        assert_eq!(
            run(0, GENESIS_TIME, GENESIS_TIME + 1, MAINNET, &times),
            Some(Bracket {
                start: Height::GENESIS,
                end: Height::GENESIS
            })
        );
        assert_eq!(
            run(0, GENESIS_TIME + 1, GENESIS_TIME + 2, MAINNET, &times),
            None
        );
    }

    #[test]
    fn near_genesis_and_whole_chain_ranges_match_brute_force() {
        let times = chain_from_deltas(&[60; 5]);
        let (low, high) = (times[1], times[4] + 1);
        assert_eq!(
            bracket_filtered(run(5, low, high, MAINNET, &times), low, high, &times),
            brute_force(low, high, &times)
        );
        let times = chain_from_deltas(&[300; 40]);
        assert_eq!(
            bracket_filtered(run(40, 0, u32::MAX, MAINNET, &times), 0, u32::MAX, &times),
            brute_force(0, u32::MAX, &times)
        );
    }

    /// A spike at the drift ceiling then blocks far below it: times go
    /// backwards, and the bracket still holds every match.
    #[test]
    fn a_spike_then_lower_blocks_is_a_superset() {
        let mut deltas = vec![60u32; 12];
        deltas.push(BLOCK_MAX_TIME_SINCE_MEDIAN);
        deltas.extend(vec![1u32; 20]);
        let times = chain_from_deltas(&deltas);
        let spike = 13usize;
        assert!(times[spike] > times[spike + 1]);
        let tip = u32::try_from(times.len() - 1).expect("fits");
        for (low, high) in [
            (times[spike] - 1, times[spike] + 1),
            (times[spike + 1], times[spike]),
            (GENESIS_TIME, times[times.len() - 1] + 1),
        ] {
            assert_eq!(
                bracket_filtered(run(tip, low, high, MAINNET, &times), low, high, &times),
                brute_force(low, high, &times),
                "[{low}, {high})"
            );
        }
        let flat = vec![GENESIS_TIME; 30];
        for (low, high) in [
            (GENESIS_TIME, GENESIS_TIME + 1),
            (GENESIS_TIME - 10, GENESIS_TIME),
            (GENESIS_TIME + 1, GENESIS_TIME + 2),
        ] {
            assert_eq!(
                bracket_filtered(run(29, low, high, MAINNET, &flat), low, high, &flat),
                brute_force(low, high, &flat)
            );
        }
    }

    /// Before the rule activates a block may be far in the future: the scan
    /// reaches genesis and keeps it.
    #[test]
    fn a_pre_activation_block_is_not_excluded() {
        let drift = Drift::activating_at(5);
        let mut times = chain_from_deltas(&[60; 8]);
        let far = GENESIS_TIME + 2_000_000;
        times[2] = far;
        let filtered = bracket_filtered(run(8, far, far + 1, drift, &times), far, far + 1, &times);
        assert!(filtered.contains(&2), "{filtered:?}");
        assert_eq!(filtered, brute_force(far, far + 1, &times));
        assert_eq!(Drift::of(Network::Regtest), Drift::activating_at(653_606));
        assert_eq!(Drift::of(Network::Mainnet), MAINNET);
    }

    /// A hole is a failure in either phase and at genesis, never a narrower
    /// bracket.
    #[test]
    fn holes_fail_loud() {
        let times = chain_from_deltas(&[60; 40]);
        let search = CandidateSearch::new(height(40), GENESIS_TIME, times[40] + 1, MAINNET);
        assert_eq!(
            run_with_hole(search.clone(), &times, 19),
            SearchStep::Failed(MissingHeader { height: height(19) })
        );
        let mut low_phase = search;
        low_phase.phase = Phase::FindLow {
            search: BinarySearch::new(1, 40),
            high_bound: height(40),
        };
        assert_eq!(
            run_with_hole(low_phase, &times, 19),
            SearchStep::Failed(MissingHeader { height: height(19) })
        );
        let genesis =
            CandidateSearch::new(Height::GENESIS, GENESIS_TIME, GENESIS_TIME + 1, MAINNET);
        assert_eq!(
            run_with_hole(genesis, &[GENESIS_TIME], 0),
            SearchStep::Failed(MissingHeader {
                height: Height::GENESIS
            })
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// The bracket filtered by time is exactly the matching blocks.
        #[test]
        fn the_bracket_matches_brute_force(
            deltas in prop::collection::vec(1u32..=BLOCK_MAX_TIME_SINCE_MEDIAN, 0..80),
            low_pick in any::<u32>(),
            width in 0u32..20_000,
        ) {
            let times = chain_from_deltas(&deltas);
            let tip = u32::try_from(times.len() - 1).expect("fits");
            let span = times[times.len() - 1].saturating_sub(times[0]).max(1);
            let low = times[0].saturating_sub(10_000) + (low_pick % (span + 20_000));
            let high = low.saturating_add(width);
            prop_assert_eq!(
                bracket_filtered(run(tip, low, high, MAINNET, &times), low, high, &times),
                brute_force(low, high, &times)
            );
        }

        /// Every match lies inside the bracket.
        #[test]
        fn the_bracket_contains_every_match(
            deltas in prop::collection::vec(1u32..=BLOCK_MAX_TIME_SINCE_MEDIAN, 0..80),
            low in any::<u32>(),
            width in 1u32..50_000,
        ) {
            let times = chain_from_deltas(&deltas);
            let tip = u32::try_from(times.len() - 1).expect("fits");
            let high = low.saturating_add(width);
            let hits = brute_force(low, high, &times);
            if let Some(first) = hits.first() {
                let bracket = run(tip, low, high, MAINNET, &times).expect("hits need a bracket");
                let (start, end) = (u32::from(bracket.start), u32::from(bracket.end));
                prop_assert!(start <= *first);
                for h in hits {
                    prop_assert!(start <= h && h <= end, "{h} outside [{start}, {end}]");
                }
            }
        }
    }
}
