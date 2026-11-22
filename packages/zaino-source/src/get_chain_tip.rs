//! Query: fetch the current best chain tip.

use std::future::Future;
use std::time::Duration;

use zaino_primitives::types::{BlockHash, Height};

use super::{FailureMode, NonDomainError, QueryError, ValidatorSource};

/// Domain error for [`GetChainTip`].
///
/// There are no domain-level failure modes for "what is the tip?" beyond
/// transport failure — the validator always has a tip. This enum exists
/// for forward compatibility (e.g. a validator that reports "still syncing").
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetChainTipError {
    /// The validator is not ready to report a tip (e.g. still syncing).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the current best chain tip (hash + height).
///
/// Maps to `getbestblockhash()` + `getblock(hash, 0)` over JSON-RPC,
/// or the equivalent ReadState query.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetChainTip: ValidatorSource + Send + Sync {
    /// Fetch current tip.
    fn get_chain_tip(
        &self,
    ) -> impl Future<
        Output = Result<(BlockHash, Height), QueryError<GetChainTipError, Self::NonDomain>>,
    > + Send;
}

/// How long between looks at a validator with no block yet.
const FIRST_BLOCK_RETRY: Duration = Duration::from_secs(3);
/// How long a validator may answer without a block before start-up fails.
const FIRST_BLOCK_WAIT: Duration = Duration::from_secs(300);

/// The validator's first chain tip: a fresh node answers RPC before its
/// first block, refusing the question, and is waited on (`waiting` is told
/// each refusal) for five minutes at most. A validator that cannot be
/// reached or authenticated to, or answers unusably, fails at once.
pub async fn first_chain_tip<S>(
    source: &S,
    mut waiting: impl FnMut(&QueryError<GetChainTipError>),
) -> Result<(BlockHash, Height), QueryError<GetChainTipError>>
where
    S: OneShotGetChainTip + ValidatorSource<NonDomain = NonDomainError>,
{
    let started = tokio::time::Instant::now();
    loop {
        let error = match source.get_chain_tip().await {
            Ok(tip) => return Ok(tip),
            Err(error) => error,
        };
        let refused = match &error {
            QueryError::Domain(GetChainTipError::NotReady) => true,
            QueryError::NonDomain(fault) => match fault.mode {
                FailureMode::RpcError(_) => true,
                FailureMode::Connection
                | FailureMode::Timeout
                | FailureMode::HttpStatus(_)
                | FailureMode::Parse
                | FailureMode::InvalidSourceData
                | FailureMode::Auth => false,
            },
        };
        if !refused || started.elapsed() >= FIRST_BLOCK_WAIT {
            return Err(error);
        }
        waiting(&error);
        tokio::time::sleep(FIRST_BLOCK_RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Refuses `refusals` times with `mode`, then serves a tip.
    struct Fresh {
        refusals: AtomicU32,
        mode: FailureMode,
    }

    impl ValidatorSource for Fresh {
        type NonDomain = NonDomainError;
    }

    impl OneShotGetChainTip for Fresh {
        async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
            let left = self.refusals.load(Ordering::Relaxed);
            if left == 0 {
                return Ok((BlockHash::from([1; 32]), Height::GENESIS));
            }
            self.refusals.store(left - 1, Ordering::Relaxed);
            Err(QueryError::NonDomain(NonDomainError::new(
                self.mode.clone(),
                "refused",
            )))
        }
    }

    fn fresh(refusals: u32, mode: FailureMode) -> Fresh {
        Fresh {
            refusals: AtomicU32::new(refusals),
            mode,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusing_validator_is_waited_on_until_it_serves_a_tip() {
        let mut waits = 0;
        let tip = first_chain_tip(&fresh(3, FailureMode::RpcError(-28)), |_| waits += 1).await;
        assert!(tip.is_ok());
        assert_eq!(waits, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_validator_that_never_serves_a_tip_fails_after_the_wait() {
        let tip = first_chain_tip(&fresh(u32::MAX, FailureMode::RpcError(-28)), |_| {}).await;
        assert!(tip.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_validator_fails_at_once() {
        let mut waits = 0;
        let tip = first_chain_tip(&fresh(1, FailureMode::Auth), |_| waits += 1).await;
        assert!(tip.is_err());
        assert_eq!(waits, 0);
    }
}
