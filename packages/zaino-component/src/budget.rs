//! A logical split of the host's cores between components.
//!
//! Each component takes its fetch and verify concurrency from one budget
//! instead of sizing itself from the core count, so the store's build and the
//! chain head's window prefetch do not both claim every core at startup.

use std::num::NonZeroUsize;

/// A component's slice of the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Share {
    /// The finalised store's build.
    ChainStore,
    /// The chain head's window prefetch.
    ChainHead,
    /// Mempool polling.
    Mempool,
    /// Chain-view passthrough reads.
    ChainView,
}

impl Share {
    /// Sixteenths of the cores this share takes.
    fn sixteenths(self) -> usize {
        match self {
            Share::ChainStore => 8,
            Share::ChainHead => 4,
            Share::Mempool => 2,
            Share::ChainView => 2,
        }
    }

    /// The floor below which the share is latency-bound rather than
    /// CPU-bound, so a small host still overlaps its round trips.
    fn floor(self) -> usize {
        match self {
            Share::ChainHead => 4,
            Share::ChainStore | Share::Mempool | Share::ChainView => 1,
        }
    }
}

/// The cores a process may use, split by [`Share`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuBudget {
    cores: NonZeroUsize,
}

impl CpuBudget {
    /// No share exceeds this, whatever the host.
    pub const MAX_PER_SHARE: usize = 16;

    /// The host's available parallelism, or one core if it cannot be read.
    pub fn detect() -> Self {
        Self::with_cores(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }

    /// A budget of exactly `cores`.
    pub fn with_cores(cores: NonZeroUsize) -> Self {
        Self { cores }
    }

    /// The cores being split.
    pub fn cores(&self) -> NonZeroUsize {
        self.cores
    }

    /// `share`'s concurrency: its slice of the cores, rounded up, never below
    /// the share's floor and never above [`Self::MAX_PER_SHARE`].
    pub fn concurrency(&self, share: Share) -> NonZeroUsize {
        let slice = self
            .cores
            .get()
            .saturating_mul(share.sixteenths())
            .div_ceil(16);
        NonZeroUsize::new(slice.max(share.floor()).min(Self::MAX_PER_SHARE))
            .unwrap_or(NonZeroUsize::MIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cores(n: usize) -> CpuBudget {
        CpuBudget::with_cores(NonZeroUsize::new(n).expect("non-zero"))
    }

    #[test]
    fn sixteen_cores_split_eight_four_two_two() {
        let budget = cores(16);
        assert_eq!(budget.concurrency(Share::ChainStore).get(), 8);
        assert_eq!(budget.concurrency(Share::ChainHead).get(), 4);
        assert_eq!(budget.concurrency(Share::Mempool).get(), 2);
        assert_eq!(budget.concurrency(Share::ChainView).get(), 2);
    }

    #[test]
    fn a_small_host_keeps_every_share_alive() {
        let budget = cores(1);
        for share in [Share::ChainStore, Share::Mempool, Share::ChainView] {
            assert_eq!(budget.concurrency(share).get(), 1);
        }
        assert_eq!(budget.concurrency(Share::ChainHead).get(), 4);
    }

    #[test]
    fn a_large_host_is_capped_per_share() {
        let budget = cores(128);
        assert_eq!(
            budget.concurrency(Share::ChainStore).get(),
            CpuBudget::MAX_PER_SHARE
        );
        assert_eq!(budget.concurrency(Share::ChainHead).get(), 16);
    }

    /// Each share rounds up, so the CPU-bound shares together may exceed the
    /// cores by at most the rounding slack of the other two.
    #[test]
    fn the_cpu_bound_shares_stay_within_rounding_of_the_cores() {
        for n in 1..=32 {
            let budget = cores(n);
            let total: usize = [Share::ChainStore, Share::Mempool, Share::ChainView]
                .into_iter()
                .map(|share| budget.concurrency(share).get())
                .sum();
            assert!(total <= n + 2, "{n} cores: CPU-bound shares sum to {total}");
        }
    }
}
