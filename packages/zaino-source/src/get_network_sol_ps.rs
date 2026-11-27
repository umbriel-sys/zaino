//! Query: estimate the network solution rate.

use std::future::Future;

use zaino_primitives::types::Height;

use super::{QueryError, ValidatorSource};

/// The blocks a solution rate is averaged over (`getnetworksolps`'s first
/// parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SolutionRateWindow {
    /// The validator's default (120 blocks): the parameter absent.
    #[default]
    Default,
    /// The difficulty averaging window at the measured height: a
    /// non-positive count.
    DifficultyAveraging,
    /// This many blocks.
    Blocks(std::num::NonZeroU32),
}

impl SolutionRateWindow {
    /// From the RPC's count: absent is the default, non-positive the
    /// difficulty averaging window.
    pub fn from_rpc(blocks: Option<i32>) -> Self {
        match blocks {
            None => Self::Default,
            Some(blocks) => u32::try_from(blocks)
                .ok()
                .and_then(std::num::NonZeroU32::new)
                .map_or(Self::DifficultyAveraging, Self::Blocks),
        }
    }

    /// The count as the RPC takes it: `None` when absent, −1 for the
    /// difficulty averaging window.
    pub fn to_rpc(self) -> Option<i64> {
        match self {
            Self::Default => None,
            Self::DifficultyAveraging => Some(-1),
            Self::Blocks(blocks) => Some(i64::from(blocks.get())),
        }
    }
}

/// Domain error for [`GetNetworkSolPs`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetNetworkSolPsError {
    /// The validator has no chain tip to measure against.
    #[error("validator not ready")]
    NotReady,
}

/// Estimate the network solution rate in solutions per second, averaged over a
/// window of blocks ending at a given height.
///
/// Returns a bare `u64` rather than a newtype: the value is a rate with no
/// invariant to protect and no other quantity it could be confused with at
/// these call sites.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetNetworkSolPs: ValidatorSource + Send + Sync {
    /// Estimate the network solution rate.
    ///
    /// `window` is the blocks averaged over; `height` the block to measure
    /// at, `None` the tip (one above the tip is the tip too).
    fn get_network_sol_ps(
        &self,
        window: SolutionRateWindow,
        height: Option<Height>,
    ) -> impl Future<Output = Result<u64, QueryError<GetNetworkSolPsError, Self::NonDomain>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::SolutionRateWindow;
    use std::num::NonZeroU32;

    /// Absent is the default; a non-positive count is the difficulty
    /// averaging window, as zebrad reads it, and is sent back as −1.
    #[test]
    fn the_window_reads_and_writes_the_rpc_count() {
        let blocks = |count| SolutionRateWindow::Blocks(NonZeroU32::new(count).expect("non-zero"));
        let cases = [
            (None, SolutionRateWindow::Default, None),
            (Some(0), SolutionRateWindow::DifficultyAveraging, Some(-1)),
            (Some(-1), SolutionRateWindow::DifficultyAveraging, Some(-1)),
            (Some(-7), SolutionRateWindow::DifficultyAveraging, Some(-1)),
            (Some(120), blocks(120), Some(120)),
        ];
        for (count, window, sent) in cases {
            assert_eq!(SolutionRateWindow::from_rpc(count), window, "{count:?}");
            assert_eq!(window.to_rpc(), sent, "{window:?}");
        }
    }
}
