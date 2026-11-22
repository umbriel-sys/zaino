//! The mempool over the validator, woken and made coherent by the chain
//! head's epoch.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadSnapshot as _};
use zaino_chain_head_service::ChainHeadSubscriber;
use zaino_mempool::MempoolConfig;
use zaino_mempool_service::{CoherenceService, MempoolService, MempoolSubscriber};
use zaino_primitives::types::{BlockHash, ChainStateEpoch, Height, TransactionId};
use zaino_source::{
    GetMempoolMetadataError, GetMempoolTxidsError, GetRawMempoolTransactionError, MempoolTxMeta,
    NonDomainError, QueryError,
};

use crate::source::ServiceSource;

pub(crate) type Mempool<S> = MempoolService<MempoolSourceAdapter<S>>;
pub(crate) type Coherence = CoherenceService<MempoolSubscriber, ChainHeadEpochAdapter>;

/// The mempool and its coherent view over the head's epoch.
pub(crate) fn spawn<S: ServiceSource>(
    source: &Arc<S>,
    head: ChainHeadSubscriber,
    config: &MempoolConfig,
    cancel: &CancellationToken,
) -> (Arc<Mempool<S>>, Arc<Coherence>) {
    let epoch = ChainHeadEpochAdapter::spawn(head, cancel.child_token());
    let mempool = MempoolService::spawn(
        MempoolSourceAdapter {
            source: Arc::clone(source),
            block_wake: epoch.epoch_wake.clone(),
        },
        config.clone(),
        cancel.child_token(),
    );
    let coherence = CoherenceService::spawn(
        mempool.subscriber(),
        epoch,
        config.clone(),
        cancel.child_token(),
    );
    (mempool, coherence)
}

/// The validator's mempool ports, woken by the head's epoch rather than the
/// validator's own block feed, so the mempool refreshes on the tip it is
/// served against.
pub(crate) struct MempoolSourceAdapter<S> {
    source: Arc<S>,
    block_wake: watch::Receiver<()>,
}

impl<S> Clone for MempoolSourceAdapter<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            block_wake: self.block_wake.clone(),
        }
    }
}

impl<S: ServiceSource> zaino_source::ValidatorSource for MempoolSourceAdapter<S> {
    type NonDomain = NonDomainError;
}

impl<S: ServiceSource> zaino_source::OneShotGetMempoolTxids for MempoolSourceAdapter<S> {
    fn get_mempool_txids(
        &self,
    ) -> impl Future<Output = Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>>> + Send
    {
        self.source.get_mempool_txids()
    }
}

impl<S: ServiceSource> zaino_source::OneShotGetMempoolMetadata for MempoolSourceAdapter<S> {
    fn get_mempool_metadata(
        &self,
    ) -> impl Future<Output = Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>>> + Send
    {
        self.source.get_mempool_metadata()
    }
}

impl<S: ServiceSource> zaino_source::OneShotGetRawMempoolTransaction for MempoolSourceAdapter<S> {
    fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>>> + Send
    {
        self.source.get_raw_mempool_transaction(txid)
    }
}

impl<S: ServiceSource> zaino_source::OneShotGetMempoolSourceTip for MempoolSourceAdapter<S> {
    fn get_mempool_source_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<std::convert::Infallible>>> + Send
    {
        self.source.get_mempool_source_tip()
    }
}

impl<S: ServiceSource> zaino_source::SubscribeBlocks for MempoolSourceAdapter<S> {
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        Some(self.block_wake.clone())
    }
}

/// The chain head's epoch as the mempool's epoch observer: the same
/// subscriber reads are answered from, so coherence freezes against the tip
/// being served. It only observes the head.
#[derive(Clone)]
pub(crate) struct ChainHeadEpochAdapter {
    head: ChainHeadSubscriber,
    epoch_wake: watch::Receiver<()>,
}

impl ChainHeadEpochAdapter {
    /// Relays the head's epoch feed as a unit wake until `cancel` or the head
    /// stops; the epoch itself is re-read on every reconcile.
    fn spawn(head: ChainHeadSubscriber, cancel: CancellationToken) -> Self {
        let (signal, epoch_wake) = watch::channel(());
        let mut updates = head.subscribe_updates();
        let stop = cancel.clone();
        let relay = async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    changed = updates.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        // a lost wake costs latency, never correctness
                        if signal.send(()).is_err() {}
                    }
                }
            }
        };
        // The mempool and coherence poll on their intervals without it, and
        // a stopped head reports itself: the relay's end costs latency only.
        tokio::spawn(zaino_component::supervised(stop, relay, |reason| {
            tracing::error!(%reason, "the chain head's epoch relay ended: the mempool refreshes on its interval alone");
        }));
        Self { head, epoch_wake }
    }
}

impl zaino_mempool::NfsEpochObserver for ChainHeadEpochAdapter {
    fn current_epoch(&self) -> Option<ChainStateEpoch> {
        Some(self.head.current().epoch())
    }

    fn subscribe_epoch_changes(&self) -> Option<watch::Receiver<()>> {
        Some(self.epoch_wake.clone())
    }
}
