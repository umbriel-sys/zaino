//! Reading from the validator, under a shared bound.
//!
//! # Why this is its own type
//!
//! Every validator read in the crate goes through here, which makes it the one
//! place that has to be right about concurrency — and the one place a future
//! gap-block cache would sit. With thousands of clients syncing the same
//! history, collapsing identical fetches is the largest win available later,
//! and it is only available if there is a single choke point to add it to.

use std::sync::Arc;

use futures::{stream, StreamExt as _, TryStreamExt as _};
use tokio::sync::Semaphore;
use zaino_primitives::types::{
    rpc::BlockHeaderVerbose, Block, BlockHash, CompactBlock, Height, TreeRoots,
};
use zaino_source::{FailureMode, NonDomainError, QueryError};

use crate::composer::config::ChainViewConfig;
use crate::error::{ChainViewError, Result};
use crate::source::ChainViewSource;

/// How often a range read that failed on the validator's connection is asked
/// again.
const CONNECTION_RETRIES: u32 = 2;
/// The wait before each of those.
const CONNECTION_RETRY_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// A validator answer, with a domain rejection folded into `None`.
///
/// The validator saying "no such block" is an answer, not a failure; a
/// transport failure carries its cause through unchanged. One helper rather
/// than a mapping per read.
pub(crate) fn miss<T, E, N>(result: core::result::Result<T, QueryError<E, N>>) -> Result<Option<T>>
where
    E: core::fmt::Debug + core::fmt::Display,
    N: std::error::Error + Into<NonDomainError>,
{
    match result {
        Ok(value) => Ok(Some(value)),
        Err(QueryError::Domain(_)) => Ok(None),
        Err(QueryError::NonDomain(error)) => Err(ChainViewError::SourceUnavailable(error.into())),
    }
}

/// A validator answer from a port whose domain errors all mean an invalid
/// request, never an absent answer.
pub(crate) fn answered<T, E, N>(result: core::result::Result<T, QueryError<E, N>>) -> Result<T>
where
    E: core::fmt::Debug + core::fmt::Display,
    N: std::error::Error + Into<NonDomainError>,
{
    match result {
        Ok(value) => Ok(value),
        Err(QueryError::Domain(error)) => Err(ChainViewError::Rejected(error.to_string())),
        Err(QueryError::NonDomain(error)) => Err(ChainViewError::SourceUnavailable(error.into())),
    }
}

/// Validator access, bounded across every client.
pub(crate) struct Fetcher<Source> {
    source: Arc<Source>,
    permits: Arc<Semaphore>,
    config: Arc<ChainViewConfig>,
}

impl<Source> Clone for Fetcher<Source> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            permits: Arc::clone(&self.permits),
            config: Arc::clone(&self.config),
        }
    }
}

impl<Source> core::fmt::Debug for Fetcher<Source> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fetcher")
            .field("available_permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

impl<Source: ChainViewSource> Fetcher<Source> {
    pub(crate) fn new(
        source: Arc<Source>,
        permits: Arc<Semaphore>,
        config: Arc<ChainViewConfig>,
    ) -> Self {
        Self {
            source,
            permits,
            config,
        }
    }

    /// The underlying source, for reads that are already one request.
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// Whether the validator may be consulted at all.
    pub(crate) fn enabled(&self) -> bool {
        self.config.passthrough_enabled
    }

    /// Refuses when the validator is switched off.
    pub(crate) fn require(&self, what: &'static str) -> Result<()> {
        if self.enabled() {
            Ok(())
        } else {
            Err(ChainViewError::ValidatorDisabled(what))
        }
    }

    /// Runs `f` holding one permit.
    ///
    /// The permit is held for the duration of the fetch and no longer — never
    /// for the length of a stream. A client streaming a million blocks holds
    /// one only while each individual block is in flight, so one long sync
    /// cannot starve anybody.
    async fn permitted<T, F>(&self, f: F) -> Result<T>
    where
        F: core::future::Future<Output = Result<T>>,
    {
        // Acquiring can only fail if the semaphore was closed, which nothing
        // does — it lives as long as the composer.
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| ChainViewError::Fatal(String::from("validator permit pool closed")))?;
        f.await
    }

    /// Maps `heights` through `fetch`, bounded per request as well as globally.
    ///
    /// Two bounds, and both matter. The shared pool stops the *fleet* from
    /// overwhelming the validator; this per-request cap stops one large range
    /// from occupying the whole pool and stalling every other client behind it.
    ///
    /// Stops at the first height the validator does not have rather than
    /// leaving a hole mid-range: a caller walking heights would read a hole as
    /// "these blocks are empty".
    ///
    /// A read that fails on the validator's connection is asked again
    /// ([`Self::attempted`]): one dropped connection must not fail a client's
    /// whole range.
    async fn fill<K, T, F, Fut>(&self, keys: Vec<K>, fetch: F) -> Result<Vec<T>>
    where
        K: Copy,
        F: Fn(K) -> Fut,
        Fut: core::future::Future<Output = Result<Option<T>>>,
    {
        let concurrency = self
            .config
            .passthrough_per_request
            .min(self.config.passthrough_permits)
            .max(1);

        let filled: Vec<Option<T>> = stream::iter(keys)
            .map(|key| self.attempted(key, &fetch))
            .buffered(concurrency)
            .try_collect()
            .await?;

        // `buffered` preserves order, so truncating at the first absence keeps
        // the run contiguous and ascending.
        Ok(filled.into_iter().map_while(|block| block).collect())
    }

    /// One key's read, under a permit; asked again, at most
    /// [`CONNECTION_RETRIES`] times, when the validator's connection failed.
    ///
    /// Only a connection failure is asked again. A timeout has already
    /// waited the read's whole allowance, a busy validator was already
    /// retried by its client, and anything the validator answered is its
    /// answer. The permit is released for the wait.
    async fn attempted<K, T, F, Fut>(&self, key: K, fetch: &F) -> Result<Option<T>>
    where
        K: Copy,
        F: Fn(K) -> Fut,
        Fut: core::future::Future<Output = Result<Option<T>>>,
    {
        let mut retries = 0u32;
        loop {
            match self.permitted(fetch(key)).await {
                Err(ChainViewError::SourceUnavailable(error))
                    if error.mode == FailureMode::Connection && retries < CONNECTION_RETRIES =>
                {
                    retries = retries.saturating_add(1);
                    tokio::time::sleep(CONNECTION_RETRY_WAIT).await;
                }
                answered => return answered,
            }
        }
    }

    /// A parsed block and its commitment roots.
    ///
    /// Two requests in sequence: the roots are asked for by the hash the block
    /// read returned, so both describe the same block. Blocks within a range
    /// still fill concurrently with one another, so this costs latency per
    /// block rather than serialising the range.
    pub(crate) async fn block_at(&self, height: Height) -> Result<Option<(Block, TreeRoots)>> {
        let Some(block) = miss(self.source.get_block(height).await)? else {
            return Ok(None);
        };
        let Some(roots) = miss(
            self.source
                .get_commitment_tree_roots(block.header.hash)
                .await,
        )?
        else {
            return Ok(None);
        };
        Ok(Some((block, roots)))
    }

    /// A compact block: one read, the block's compact data and its tree
    /// sizes as one answer about one block. `pools` goes to the source, as
    /// it goes to the store's and the chain head's reads, so a caller
    /// filtering to one pool gets the same shape from every provider.
    pub(crate) async fn compact_at(
        &self,
        height: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Option<CompactBlock>> {
        miss(self.source.get_compact_block(height, pools).await)
    }

    /// Consensus bytes at `height`.
    pub(crate) async fn raw_block_at(&self, height: Height) -> Result<Option<Vec<u8>>> {
        miss(self.source.get_raw_block(height).await)
    }

    /// Fills `heights` with parsed blocks and their roots.
    pub(crate) async fn fill_blocks(
        &self,
        heights: Vec<Height>,
    ) -> Result<Vec<(Block, TreeRoots)>> {
        self.fill(heights, |height| self.block_at(height)).await
    }

    /// Fills `heights` with compact blocks.
    pub(crate) async fn fill_compact(
        &self,
        heights: Vec<Height>,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<CompactBlock>> {
        self.fill(heights, |height| self.compact_at(height, pools))
            .await
    }

    /// Fills `heights` with consensus bytes.
    pub(crate) async fn fill_raw_blocks(&self, heights: Vec<Height>) -> Result<Vec<Vec<u8>>> {
        self.fill(heights, |height| self.raw_block_at(height)).await
    }

    /// Fills `hashes` with consensus bytes.
    pub(crate) async fn fill_raw_blocks_by_hash(
        &self,
        hashes: Vec<BlockHash>,
    ) -> Result<Vec<Vec<u8>>> {
        self.fill(hashes, |hash| async move {
            miss(self.source.get_raw_block_by_hash(hash).await)
        })
        .await
    }

    /// A block's header, for an id-addressed read no provider covers.
    pub(crate) async fn block_header(&self, hash: BlockHash) -> Result<Option<BlockHeaderVerbose>> {
        self.permitted(async { miss(self.source.get_block_header(hash).await) })
            .await
    }
}
