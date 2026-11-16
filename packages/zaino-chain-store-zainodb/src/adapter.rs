//! This backend, as the `zaino-chain-store` ports.
//!
//! # This module implements ports; it does not define them
//!
//! The driven ports live in the `zaino-chain-store` domain crate, which is
//! where a consumer should read their contracts. An adapter has none of its
//! own — it satisfies someone else's — so what is here is `impl <port> for
//! DbReader<T>` and `impl <port> for FinalisedState<T>`, plus the conversions
//! those impls need. The module was called `ports` until it was pointed out
//! that the name reads as "the port contracts are defined here", which inverts
//! the layering it exists to express.
//!
//! # Where things are
//!
//! - `error_map` — this backend's failures, as the domain names them.
//! - `to_domain` — reading: what is on disk, as the domain names it.
//! - `from_domain` — writing: what the domain hands over, as this stores it.
//! - `history` — the feature-gated transparent-history port.
//!
//! Named in code spans rather than doc links because the modules are private:
//! an intra-doc link to one resolves only under `--document-private-items`, so
//! linking them breaks a plain `cargo doc`.
//!
//! The two conversion directions are separate modules rather than one, because
//! they are separate concerns that happen to be inverse: one is what a read
//! must survive, the other what a write must preserve. They are *not* named for
//! the types they produce — `StoredBlock` is a **domain** type, so a module
//! called `into_stored` would hold the conversions *out of* this backend, which
//! is the opposite of what the name suggests. `to_domain` and `from_domain`
//! name the direction and cannot be read backwards.
//!
//! # What this module does *not* do
//!
//! It does not re-implement any read. Every method delegates to the existing
//! reader and converts the answer. Where a domain method has no single backend
//! counterpart — `unspent_output`, which is an existence check and a spend
//! check — the composition is here, because it is a question about the domain
//! rather than about LMDB.

mod error_map;
mod from_domain;
#[cfg(feature = "transparent_address_history_experimental")]
mod history;
mod to_domain;

pub use from_domain::indexed_block_from_stored;
pub use to_domain::{domain_outpoint, stored_tx_out};

pub(crate) use to_domain::domain_block_ref;

use error_map::{chain_store_error, chain_store_source_error};
use from_domain::{stored_hash, stored_height, stored_outpoint, tx_location};
use to_domain::{
    block_tx_position, domain_hash, domain_height, domain_txid, store_capabilities, store_schema,
    stored_block, stored_tx_outs,
};

use core::future::Future;

use zaino_chain_store::{
    ChainStoreError, ChainStoreIngest, ChainStoreReader, ChainStoreService, ChainStoreSource,
    ChainStoreSourceError, CompactBlockRead, PoolFilter, SpenderRef, SpentOutputIndex,
    StoreCapabilities, StoreSchema, StoreWatermark, StoredHeader, StoredTxOut, TransactionIndex,
    TreeRootsIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_primitives::types::{
    AbsoluteChainWork, BlockHash as DomainBlockHash, BlockHeader, BlockTxPosition, CompactBlock,
    Height as DomainHeight, Outpoint as DomainOutpoint, PreIndexCompactTx, SignedZatoshis,
    TransactionId, TreeRoots,
};

/// A finalised block as this backend's rows express it: the legacy shape the
/// `zaino-chain-store` ports once served, kept here for the legacy
/// `IndexedBlock` conversion until the crate is deleted.
#[derive(Debug, Clone)]
pub struct StoredBlock {
    /// The block's header.
    pub header: BlockHeader,
    /// Per-transaction indexed data, in block order.
    pub transactions: Vec<StoredTx>,
    /// Commitment tree roots and sizes after this block is applied.
    pub tree_roots: TreeRoots,
    /// Cumulative work from genesis to this block.
    pub chainwork: AbsoluteChainWork,
}

/// A transaction in a stored block: the compact transaction plus the per-pool
/// value balances this backend's rows persist beside it.
#[derive(Debug, Clone)]
pub struct StoredTx {
    /// The compact transaction.
    pub compact: PreIndexCompactTx,
    /// Net Sapling value balance, `None` without a Sapling component.
    pub sapling_value: Option<SignedZatoshis>,
    /// Net Orchard value balance, `None` without an Orchard component.
    pub orchard_value: Option<SignedZatoshis>,
    /// Net Ironwood value balance, `None` without an Ironwood component.
    pub ironwood_value: Option<SignedZatoshis>,
}

impl StoredTx {
    /// A transaction with no shielded value movement recorded.
    pub fn transparent_only(compact: PreIndexCompactTx) -> Self {
        Self {
            compact,
            sapling_value: None,
            orchard_value: None,
            ironwood_value: None,
        }
    }
}
use zaino_status::StatusType;

use crate::error::StoreError;
use crate::store::reader::DbReader;
use crate::store::FinalisedState;
use crate::types::{Height, Outpoint, TransactionHash};

impl<T: ChainStoreSource> ChainStoreReader for DbReader<T> {
    fn watermark(&self) -> StoreWatermark {
        self.inner.watermark()
    }

    fn capabilities(&self) -> StoreCapabilities {
        store_capabilities::<Self>(self.inner.capability())
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        let metadata = self.get_metadata().await.map_err(chain_store_error)?;
        Ok(store_schema(&metadata))
    }

    #[tracing::instrument(skip(self), fields(height = %height))]
    async fn block_hash(
        &self,
        height: DomainHeight,
    ) -> Result<Option<DomainBlockHash>, ChainStoreError> {
        self.bounded(height)?;
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "block_hash"),
        );
        Ok(DbReader::get_block_hash(self, stored_height(height))
            .await
            .map_err(chain_store_error)?
            .map(domain_hash))
    }

    #[tracing::instrument(skip(self), fields(hash = %hash))]
    async fn block_height(
        &self,
        hash: DomainBlockHash,
    ) -> Result<Option<DomainHeight>, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "block_height"),
        );
        match DbReader::get_block_height(self, stored_hash(hash))
            .await
            .map_err(chain_store_error)?
        {
            Some(height) => Ok(Some(domain_height(height)?)),
            None => Ok(None),
        }
    }

    /// Projected from this backend's whole-block rows: it holds no header
    /// table of its own.
    async fn headers_chunk(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Vec<StoredHeader>, ChainStoreError> {
        Ok(self
            .stored_range(start, end)
            .await?
            .into_iter()
            .map(|block| StoredHeader {
                header: block.header,
                chainwork: block.chainwork,
            })
            .collect())
    }

    async fn chainwork(
        &self,
        height: DomainHeight,
    ) -> Result<Option<AbsoluteChainWork>, ChainStoreError> {
        self.bounded(height)?;
        Ok(self
            .stored_range(height, height)
            .await?
            .first()
            .map(|block| block.chainwork))
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// The stored blocks in `start..=end`, clamped to the watermark.
    async fn stored_range(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Vec<StoredBlock>, ChainStoreError> {
        let Some((start, end)) = self.clamped_range(start, end)? else {
            return Ok(Vec::new());
        };
        self.get_stored_block_range(start, end)
            .await
            .map_err(chain_store_error)?
            .into_iter()
            .map(stored_block)
            .collect()
    }
}

impl<T: ChainStoreSource> TreeRootsIndex for DbReader<T> {
    async fn tree_roots_chunk(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Vec<TreeRoots>, ChainStoreError> {
        Ok(self
            .stored_range(start, end)
            .await?
            .into_iter()
            .map(|block| block.tree_roots)
            .collect())
    }
}

/// A reader reports the store's status, not one of its own.
///
/// It holds the `FinalisedState` it reads from, so there is one status and one
/// name however many handles exist. A supervisor observing a reader and a
/// service sees the same component.
impl<T: ChainStoreSource> StatusSource for DbReader<T> {
    fn status(&self) -> ComponentStatus {
        component_status(self.inner.name(), DbReader::status(self))
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// Rejects a height the store cannot answer for yet.
    ///
    /// Above the watermark is not a miss: the block probably exists and simply
    /// is not finalised, so a caller must route the question elsewhere rather
    /// than conclude the chain has no such block.
    fn bounded(&self, height: DomainHeight) -> Result<(), ChainStoreError> {
        let watermark = self.inner.watermark();
        if watermark.covers(height) {
            return Ok(());
        }
        // A passthrough store is not bounded by what it holds, because it is
        // not answering from what it holds: the read goes to the validator, and
        // the validator has the block. The watermark still describes the
        // durable rows — that is what it is for — but using it as a *limit*
        // here would refuse a question this store can answer perfectly well,
        // which is exactly the case a store that is still building is in.
        if watermark.provenance == zaino_chain_store::Provenance::Passthrough {
            return Ok(());
        }
        // No tip at all is not "above the watermark" — there is no watermark to
        // be above. A store still opening or still empty is transiently unable
        // to answer, which is what the caller needs to know.
        match watermark.tip {
            Some(tip) => Err(ChainStoreError::AboveWatermark {
                requested: height,
                watermark: tip.height,
            }),
            None => Err(ChainStoreError::NotReady),
        }
    }
}

/// How many blocks one read transaction covers.
///
/// The existing compact-block walk uses the same figure, and for the same
/// reason: it bounds how long a reader slot is held without making the
/// per-transaction cost dominate. Chunk boundaries carry no meaning — a
/// consumer must not read anything into where one ends.
const BLOCKS_PER_READ_TRANSACTION: u32 = 1024;

/// Splits `start..=end` into chunks and reads each one.
///
/// Sequential rather than concurrent: the chunks are contiguous and the
/// consumer wants them in order, so overlapping reads would buy nothing and
/// would hold several reader slots at once. The stream stops at the first
/// error — a range with a hole in the middle is not a range.
fn chunked<B, F, Fut>(
    range: Option<(Height, Height)>,
    read: F,
) -> impl futures::Stream<Item = Result<Vec<B>, ChainStoreError>> + Send
where
    B: Send + 'static,
    F: FnMut(Height, Height) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<B>, ChainStoreError>> + Send + 'static,
{
    // `range` is `None` when the whole request sits above the watermark. The
    // empty case is folded in here rather than returned as
    // `stream::empty()` from the caller, because the port hands back one
    // opaque stream type and two arms returning different types could not
    // both be it.
    let (cursor, end) = match range {
        Some((start, end)) => (Some(start), end),
        None => (None, Height(0)),
    };
    // The reader travels in the unfold's state rather than being borrowed by
    // the closure: the future the closure returns outlives the call, so a
    // borrow would tie the stream's lifetime to this frame.
    futures::stream::try_unfold((cursor, read), move |(cursor, mut read)| async move {
        let Some(from) = cursor else {
            return Ok(None);
        };
        let to = Height(
            from.0
                .saturating_add(BLOCKS_PER_READ_TRANSACTION - 1)
                .min(end.0),
        );
        let chunk = read(from, to).await?;
        let next = (to.0 < end.0).then(|| Height(to.0 + 1));
        Ok(Some((chunk, (next, read))))
    })
}

impl<T: ChainStoreSource> DbReader<T> {
    /// Narrows `start..=end` to what this store can answer for.
    ///
    /// A range extending above the watermark is truncated rather than refused,
    /// so a consumer merging with the recent window asks both halves the same
    /// question and lets each answer what it holds. `None` means the whole range
    /// is above the watermark, which is an empty answer rather than an error —
    /// the other half has all of it.
    ///
    /// A range whose start is above its end is rejected: ranges are ascending,
    /// and a descending one is a caller mistake rather than an empty set.
    fn clamped_range(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Option<(Height, Height)>, ChainStoreError> {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }

        let watermark = self.inner.watermark();

        // Passthrough answers from the validator, so there is nothing to clamp
        // to — see [`Self::bounded`]. The ascending check above still applies,
        // because that one is about the request rather than about coverage.
        if watermark.provenance == zaino_chain_store::Provenance::Passthrough {
            return Ok(Some((stored_height(start), stored_height(end))));
        }

        let Some(tip) = watermark.tip else {
            return Err(ChainStoreError::NotReady);
        };
        if start > tip.height {
            return Ok(None);
        }
        Ok(Some((
            stored_height(start),
            stored_height(end.min(tip.height)),
        )))
    }
}

impl<T: ChainStoreSource> TransactionIndex for DbReader<T> {
    #[tracing::instrument(skip(self), fields(txid = %txid))]
    async fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "tx_position"),
        );
        match self
            .get_tx_location(&TransactionHash((*txid).into()))
            .await
            .map_err(chain_store_error)?
        {
            Some(location) => Ok(Some(block_tx_position(location)?)),
            None => Ok(None),
        }
    }

    #[tracing::instrument(skip(self), fields(position = ?position))]
    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        self.bounded(position.height)?;
        let Some(location) = tx_location(position) else {
            return Ok(None);
        };
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "txid_at"),
        );
        // The backend errors on a miss where the domain answers `None`: asking
        // about a position past the end of a block is a reasonable question.
        match self.get_txid(location).await {
            Ok(txid) => Ok(Some(domain_txid(txid))),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}

impl<T: ChainStoreSource> SpentOutputIndex for DbReader<T> {
    #[tracing::instrument(skip(self, outpoints), fields(outpoints = outpoints.len()))]
    async fn outpoint_spenders(
        &self,
        outpoints: &[DomainOutpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "outpoint_spenders"),
        );
        let stored: Vec<Outpoint> = outpoints.iter().map(stored_outpoint).collect();
        let locations = DbReader::get_outpoint_spenders(self, stored)
            .await
            .map_err(chain_store_error)?;

        // The domain answer carries the spender's txid as well as its position,
        // because every caller resolves one to the other immediately. Doing it
        // here costs the same reads and halves the traffic across the seam.
        let mut spenders = Vec::with_capacity(locations.len());
        for location in locations {
            let Some(location) = location else {
                spenders.push(None);
                continue;
            };
            let txid = self.get_txid(location).await.map_err(chain_store_error)?;
            spenders.push(Some(SpenderRef {
                position: block_tx_position(location)?,
                txid: domain_txid(txid),
            }));
        }
        Ok(spenders)
    }

    #[tracing::instrument(skip(self, outpoints), fields(outpoints = outpoints.len()))]
    async fn previous_outputs(
        &self,
        outpoints: &[DomainOutpoint],
    ) -> Result<Vec<Option<StoredTxOut>>, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "previous_outputs"),
        );
        let mut outputs = Vec::with_capacity(outpoints.len());
        for outpoint in outpoints {
            outputs.push(self.previous_output(outpoint).await?);
        }
        Ok(outputs)
    }

    #[tracing::instrument(skip(self), fields(outpoint = ?outpoint))]
    async fn unspent_output(
        &self,
        outpoint: DomainOutpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "unspent_output"),
        );
        let Some(output) = self.previous_output(&outpoint).await? else {
            return Ok(None);
        };
        let spent = DbReader::get_outpoint_spender(self, stored_outpoint(&outpoint))
            .await
            .map_err(chain_store_error)?
            .is_some();

        Ok((!spent).then_some(output))
    }

    #[tracing::instrument(skip(self), fields(position = ?position))]
    async fn transparent_outputs(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<Vec<StoredTxOut>>, ChainStoreError> {
        self.bounded(position.height)?;
        let Some(location) = tx_location(position) else {
            return Ok(None);
        };
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "transparent_outputs"),
        );
        match DbReader::get_transparent(self, location)
            .await
            .map_err(chain_store_error)?
        {
            Some(transparent) => Ok(Some(stored_tx_outs(&transparent)?)),
            None => Ok(None),
        }
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// One outpoint's output, with a miss as `None`.
    ///
    /// The backend errors when the creating transaction is absent; the domain
    /// treats that as an answer, because an outpoint naming a transaction this
    /// store does not hold is exactly what a caller merging across the seam
    /// expects to see.
    async fn previous_output(
        &self,
        outpoint: &DomainOutpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        match DbReader::get_previous_output(self, stored_outpoint(outpoint)).await {
            Ok(output) => Ok(Some(stored_tx_out(&output)?)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}

impl<T: ChainStoreSource> TxOutSetIndex for DbReader<T> {
    #[tracing::instrument(skip(self))]
    async fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "txout_set"),
        );
        let accumulator = self
            .get_tx_out_set_info_accumulator()
            .await
            .map_err(chain_store_error)?;

        // The stored row *is* the domain value; only its encoding is this
        // backend's. The commitment it carries is defined by
        // `zaino_chain_store::txout_set`, which is also what maintains it — so
        // this hands the value over rather than restating it.
        Ok(accumulator.into_business())
    }
}

impl<T: ChainStoreSource> CompactBlockRead for DbReader<T> {
    #[tracing::instrument(skip(self, pools), fields(start = %start, end = %end))]
    async fn compact_chunk(
        &self,
        start: DomainHeight,
        end: DomainHeight,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        let Some((start, end)) = self.clamped_range(start, end)? else {
            return Ok(Vec::new());
        };

        // The wallet-sync hot path: a syncing wallet spends almost all of its
        // time here, so this is the read whose latency a dashboard needs.
        let _timer = crate::timer::Timer::start(
            metrics::histogram!(crate::metric_names::DB_READ_SECONDS, "op" => "compact_chunk"),
        );

        DbReader::get_compact_block_range(self, start, end, pools)
            .await
            .map_err(chain_store_error)
    }

    async fn compact_stream(
        &self,
        start: DomainHeight,
        end: DomainHeight,
        pools: PoolFilter,
    ) -> Result<
        impl futures::Stream<Item = Result<Vec<CompactBlock>, ChainStoreError>> + Send + use<T>,
        ChainStoreError,
    > {
        let reader = self.clone();
        Ok(chunked(self.clamped_range(start, end)?, move |from, to| {
            let reader = reader.clone();
            async move {
                DbReader::get_compact_block_range(&reader, from, to, pools)
                    .await
                    .map_err(chain_store_error)
            }
        }))
    }
}

impl<T: ChainStoreSource> ChainStoreService for FinalisedState<T> {
    type Reader = DbReader<T>;

    fn reader(&self) -> Self::Reader {
        FinalisedState::reader(self)
    }

    fn subscribe_watermark(&self) -> tokio::sync::watch::Receiver<StoreWatermark> {
        self.subscribe_watermark()
    }
}

impl<T: ChainStoreSource> ChainStoreIngest for FinalisedState<T> {
    async fn build_to(&self, target: DomainHeight) -> Result<(), ChainStoreSourceError> {
        FinalisedState::build_to(self, stored_height(target))
            .await
            .map_err(chain_store_source_error)
    }

    async fn rewind_to(&self, height: DomainHeight) -> Result<(), ChainStoreError> {
        FinalisedState::rewind_to(self, stored_height(height))
            .await
            .map_err(chain_store_error)
    }

    fn wait_until_built(&self) -> impl Future<Output = ()> + Send {
        self.wait_until_synced()
    }

    async fn shutdown(&self) -> Result<(), ChainStoreError> {
        FinalisedState::shutdown(self)
            .await
            .map_err(chain_store_error)
    }
}

impl<T: ChainStoreSource> StatusSource for FinalisedState<T> {
    fn status(&self) -> ComponentStatus {
        component_status(self.name(), FinalisedState::status(self))
    }
}

/// This store's fused status, as the two axes a component reports.
///
/// Transitional, and deliberately the only place the two vocabularies meet.
/// The store tracks the fused [`StatusType`] throughout; nothing inside it
/// changes shape, and when it is rewritten to hold a phase and a condition
/// separately this function goes rather than being threaded further in.
///
/// The mapping is exact but for the two error states. They are *health* in the
/// split model, but in the fused one they overwrite the phase — a `Ready`
/// store that hits a recoverable fault stops recording that it was ready — so
/// the phase they came from is not recoverable here. A fixed phase is chosen,
/// erring towards caution: a degraded store reports `Syncing` rather than
/// claiming readiness it may not have, and a broken one reports `Offline`
/// rather than a phase it is not really in.
///
/// `Busy` has no counterpart either; the component crate defers the load axis.
/// It is only ever produced when the router cannot resolve a backend for core
/// reads, which is a degraded store rather than a loaded one — so it maps that
/// way, and not to the readiness the fused model gave it.
fn component_status(name: ComponentName, status: StatusType) -> ComponentStatus {
    let (lifecycle, health) = match status {
        StatusType::Spawning => (Lifecycle::Spawning, Health::Healthy),
        StatusType::Syncing => (Lifecycle::Syncing, Health::Healthy),
        StatusType::Ready => (Lifecycle::Ready, Health::Healthy),
        StatusType::Closing => (Lifecycle::Closing, Health::Healthy),
        StatusType::Offline => (Lifecycle::Offline, Health::Offline),
        StatusType::Busy | StatusType::RecoverableError => {
            (Lifecycle::Syncing, Health::Recoverable)
        }
        StatusType::CriticalError => (Lifecycle::Offline, Health::Critical),
    };

    ComponentStatus::new(name, lifecycle, health)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every port this backend claims to implement is actually implemented.
    ///
    /// A bound check, not a behaviour test, and it earns its place: nothing else
    /// notices when a port is declared in the domain crate and never satisfied
    /// here. The failure it catches is a consumer being unable to name a
    /// capability the store advertises — which shows up at wiring time in
    /// another crate, a long way from the cause.
    #[test]
    fn the_backend_satisfies_every_port_it_advertises() {
        fn reader<R: ChainStoreReader + TreeRootsIndex + CompactBlockRead>() {}
        fn indexes<R: TransactionIndex + SpentOutputIndex + TxOutSetIndex>() {}
        fn service<S: ChainStoreService + ChainStoreIngest>() {}
        #[cfg(feature = "transparent_address_history_experimental")]
        fn history<R: zaino_chain_store::TransparentHistoryIndex>() {}

        type Validator = zaino_source_zebra::ZebraValidator;

        reader::<DbReader<Validator>>();
        indexes::<DbReader<Validator>>();
        service::<FinalisedState<Validator>>();
        #[cfg(feature = "transparent_address_history_experimental")]
        history::<DbReader<Validator>>();
    }
}
