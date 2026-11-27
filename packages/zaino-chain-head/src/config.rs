//! How the ChainHead runtime is configured.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::Duration;

/// How far below the tip ChainHead retains blocks, and how hard it polls.
///
/// [`Default`] gives the consensus reorg bound, which is what production wants.
/// `max_depth` is still configurable because the window is a deployment fact:
/// tests need a tractable depth to exercise a *moving* seam against short
/// chains, and they set it here rather than through a compile-time feature this
/// crate would otherwise have to carry.
///
/// # Every knob is `NonZero`, and that is not uniformity for its own sake
///
/// Zero is meaningless for all five, so it is made unrepresentable rather than
/// checked at startup — the failures it produces are silent or late:
///
/// - `max_depth` of zero retains only the tip, so the chain head could not
///   observe the reorgs it exists to hold.
/// - `poll_interval` of zero turns the writer loop into a spin against the
///   validator. It does not panic — the loop sleeps rather than ticking — which
///   is worse than panicking, because the damage lands on the node being polled.
/// - `initial_backoff` or `max_backoff` of zero defeats the backoff entirely:
///   `backoff * 2` stays zero, so a failing validator is retried in a tight
///   loop for as long as it keeps failing.
/// - `max_consecutive_failures` of zero and of one are the same thing — the
///   count is compared after it is incremented — so zero buys no behaviour that
///   one does not already give.
///
/// Contrast a knob where zero *is* meaningful, which would stay plain: there is
/// none here. If one is added, it keeps its zero rather than taking `NonZero`
/// for consistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHeadConfig {
    max_depth: NonZeroU32,
    poll_interval_ms: NonZeroU64,
    initial_backoff_ms: NonZeroU64,
    max_backoff_ms: NonZeroU64,
    max_consecutive_failures: NonZeroU32,
    fetch_concurrency: NonZeroUsize,
}

// a reorg bound of zero would mean the chain never reorganises
const _: () = assert!(zaino_consensus::MAX_NONFINALISED_DEPTH > 0);

/// The consensus reorg bound, as a `NonZeroU32` (non-zero, asserted above at
/// compile time, so the fallback is never taken).
fn default_max_depth() -> NonZeroU32 {
    NonZeroU32::new(zaino_consensus::MAX_NONFINALISED_DEPTH).unwrap_or(NonZeroU32::MIN)
}

/// Milliseconds as a `NonZeroU64`, for the literal defaults below (never
/// zero, so the fallback is never taken).
fn ms(millis: u64) -> NonZeroU64 {
    NonZeroU64::new(millis).unwrap_or(NonZeroU64::MIN)
}

impl Default for ChainHeadConfig {
    fn default() -> Self {
        Self {
            max_depth: default_max_depth(),
            poll_interval_ms: ms(1_000),
            initial_backoff_ms: ms(500),
            max_backoff_ms: ms(30_000),
            max_consecutive_failures: NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN),
            fetch_concurrency: zaino_component::CpuBudget::detect()
                .concurrency(zaino_component::Share::ChainHead),
        }
    }
}

impl ChainHeadConfig {
    /// The default configuration, retaining `max_depth` blocks instead of the
    /// consensus reorg bound.
    ///
    /// For callers that need a shallower window than production — a test
    /// exercising eviction against a chain far shorter than
    /// [`MAX_NONFINALISED_DEPTH`](zaino_consensus::MAX_NONFINALISED_DEPTH),
    /// where the real depth would keep every block it ever saw and the seam
    /// would never move.
    pub fn with_max_depth(max_depth: NonZeroU32) -> Self {
        Self {
            max_depth,
            ..Self::default()
        }
    }

    /// Blocks retained below the canonical tip.
    ///
    /// The retention floor is `best_tip.height - max_depth`. This also bounds
    /// every ancestry walk: neither reorg handling nor competing-branch
    /// resolution should recurse further back than the window it maintains.
    pub fn max_depth(&self) -> u32 {
        self.max_depth.get()
    }

    /// How often to re-read the source when nothing wakes ChainHead sooner.
    ///
    /// Correctness never depends on this: a wake is a latency hint, and
    /// ChainHead re-reads the source on every wake regardless.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.get())
    }

    /// Delay before the first retry after a source failure. Doubles on each
    /// consecutive failure up to [`max_backoff`](Self::max_backoff).
    pub fn initial_backoff(&self) -> Duration {
        Duration::from_millis(self.initial_backoff_ms.get())
    }

    /// Ceiling on the doubling backoff.
    pub fn max_backoff(&self) -> Duration {
        Duration::from_millis(self.max_backoff_ms.get())
    }

    /// Consecutive source failures tolerated before ChainHead reports itself
    /// critically failed.
    ///
    /// A validator that is briefly unreachable should not take ChainHead down;
    /// one that stays unreachable should not be reported as healthy.
    pub fn max_consecutive_failures(&self) -> u32 {
        self.max_consecutive_failures.get()
    }

    /// Set the poll interval, in milliseconds.
    pub fn set_poll_interval_ms(&mut self, millis: NonZeroU64) {
        self.poll_interval_ms = millis;
    }

    /// Set the first retry delay, in milliseconds.
    pub fn set_initial_backoff_ms(&mut self, millis: NonZeroU64) {
        self.initial_backoff_ms = millis;
    }

    /// Set the backoff ceiling, in milliseconds.
    pub fn set_max_backoff_ms(&mut self, millis: NonZeroU64) {
        self.max_backoff_ms = millis;
    }

    /// Set how many consecutive failures are tolerated.
    pub fn set_max_consecutive_failures(&mut self, failures: NonZeroU32) {
        self.max_consecutive_failures = failures;
    }

    /// Heights fetched ahead of the link while the window is built. Default:
    /// the chain head's share of [`zaino_component::CpuBudget`].
    pub fn fetch_concurrency(&self) -> NonZeroUsize {
        self.fetch_concurrency
    }

    /// Set how many heights are fetched ahead of the link.
    pub fn set_fetch_concurrency(&mut self, heights: NonZeroUsize) {
        self.fetch_concurrency = heights;
    }
}
