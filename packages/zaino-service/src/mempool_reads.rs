//! Mempool reads (D6-02): the tip-agnostic set for listings, the coherent
//! view for anything placed relative to this snapshot's tip.

use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use zaino_chain::{BlockRead as _, ChainTxPosition, ChainViewSnapshot as _, TransactionRead as _};
use zaino_mempool::{CoherentSnapshot, MempoolEntry, TipAwareMempool as _};
use zaino_mempool_service::{CoherentSubscriber, MempoolInfo, MempoolSubscriber};
use zaino_primitives::types::{ChainStateEpoch, Height, TransactionId, TransactionLocation};

use crate::error::ServiceError;
use crate::reads::ServiceSnapshot;
use crate::source::ServiceSource;

type Result<T> = core::result::Result<T, ServiceError>;

/// A transaction's bytes and the height it was, or would be, mined at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedTransaction {
    /// Its consensus bytes.
    pub bytes: Vec<u8>,
    /// Mined at this height, or, from the mempool, to be mined at it (the
    /// tip's successor).
    pub height: Height,
    /// Whether it came from the mempool.
    pub mempool: bool,
}

/// Where a transaction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// In a block.
    Block(ChainTxPosition),
    /// In the mempool, to be mined at the height given (`None` when Zaino no
    /// longer holds the tip the mempool is for).
    Mempool(Option<Height>),
}

/// Where a transaction is, as one snapshot sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionStatus {
    /// On the snapshot's best chain, or in a mempool coherent with it.
    pub best: Option<Placement>,
    /// Off it: competing blocks, or a mempool for another tip.
    pub elsewhere: Vec<Placement>,
}

impl<S: ServiceSource> ServiceSnapshot<S> {
    /// The mempool's txids, whatever the tip.
    pub fn mempool_txids(&self) -> Arc<[TransactionId]> {
        self.mempool.get_txids()
    }

    /// The mempool's size and cost, whatever the tip.
    pub fn mempool_info(&self) -> MempoolInfo {
        self.mempool.get_mempool_info()
    }

    /// The mempool's entries but those whose txid ends in one of `exclude`;
    /// a malformed suffix is refused, never trimmed.
    pub fn mempool_entries(&self, exclude: &[Vec<u8>]) -> Result<Vec<Arc<MempoolEntry>>> {
        entries(&self.mempool, exclude)
    }

    /// The coherent mempool's transactions until the tip moves, and the new
    /// ones as they arrive; with `pinned`, only if coherent with this
    /// snapshot (`None` otherwise). A lag ends it with an error, never
    /// silently.
    pub fn mempool_stream(
        &self,
        pinned: bool,
    ) -> Option<impl Stream<Item = Result<bytes::Bytes>> + Send + '_> {
        coherent_stream(&self.coherence, pinned.then(|| self.chain().epoch()))
    }

    /// A transaction's bytes: from the mempool when coherent with this
    /// snapshot, else from the chain.
    pub async fn transaction(&self, txid: TransactionId) -> Result<Option<ServedTransaction>> {
        let coherent = self.coherence.coherent_snapshot();
        match self.coherent_epoch(&coherent) {
            Some(epoch) => {
                if let Some(entry) = coherent.get(&txid) {
                    return Ok(Some(ServedTransaction {
                        bytes: entry.wire_bytes().to_vec(),
                        height: successor(epoch.best_tip.height)?,
                        mempool: true,
                    }));
                }
            }
            // the core set can lag the chain and still list a mined txid
            None if self.mempool.contains_txid(&txid)
                && self.chain().transaction_locations(txid).await?.is_empty() =>
            {
                return Err(ServiceError::MempoolIncoherent);
            }
            None => {}
        }
        let Some(transaction) = self.chain().raw_transaction(txid).await? else {
            return Ok(None);
        };
        let height = match transaction.location {
            TransactionLocation::BestChain(height) => height,
            TransactionLocation::NonBestChain => {
                let locations = self.chain().transaction_locations(txid).await?;
                match locations
                    .best_chain
                    .or_else(|| locations.non_best_chain.first().copied())
                {
                    Some(position) => position.block.height,
                    None => return Ok(None),
                }
            }
            TransactionLocation::Mempool => return Ok(None),
        };
        Ok(Some(ServedTransaction {
            bytes: transaction.bytes,
            height,
            mempool: false,
        }))
    }

    /// Where a transaction is: its blocks, and the mempool, placed against
    /// this snapshot when coherent with it, else at the successor of the
    /// mempool's tip as the chain holds it now.
    pub async fn transaction_status(&self, txid: TransactionId) -> Result<TransactionStatus> {
        let locations = self.chain().transaction_locations(txid).await?;
        let mut status = TransactionStatus {
            best: locations.best_chain.map(Placement::Block),
            elsewhere: locations
                .non_best_chain
                .into_iter()
                .map(Placement::Block)
                .collect(),
        };
        if !self.mempool.contains_txid(&txid) {
            return Ok(status);
        }
        let coherent = self.coherence.coherent_snapshot();
        if self.coherent_epoch(&coherent).is_some() {
            if status.best.is_some() {
                return Err(ServiceError::MinedAndInMempool(txid));
            }
            status.best = Some(Placement::Mempool(Some(successor(self.tip().height)?)));
        } else {
            // the mempool tip's successor, if the chain holds that tip now
            let target = match coherent.valid_for {
                Some(epoch) => match self
                    .view
                    .snapshot()
                    .block_height(epoch.best_tip.hash)
                    .await?
                {
                    Some(height) => Some(successor(height)?),
                    None => None,
                },
                None => None,
            };
            status.elsewhere.push(Placement::Mempool(target));
        }
        Ok(status)
    }

    /// This snapshot's epoch, if `coherent` is valid for it.
    fn coherent_epoch(&self, coherent: &CoherentSnapshot) -> Option<ChainStateEpoch> {
        let epoch = self.chain().epoch();
        coherent.is_valid_for_snapshot(epoch).then_some(epoch)
    }
}

/// The set's entries but those whose txid ends in one of `exclude`.
pub(crate) fn entries(
    mempool: &MempoolSubscriber,
    exclude: &[Vec<u8>],
) -> Result<Vec<Arc<MempoolEntry>>> {
    let suffixes = mempool
        .validate_exclude_suffixes(exclude)
        .map_err(|error| ServiceError::InvalidParameter(error.to_string()))?;
    Ok(mempool.get_filtered_entries(&suffixes))
}

/// The coherent mempool's stream until the tip moves; with `epoch`, only if
/// coherent with it.
pub(crate) fn coherent_stream(
    coherence: &CoherentSubscriber,
    epoch: Option<ChainStateEpoch>,
) -> Option<impl Stream<Item = Result<bytes::Bytes>> + Send + '_> {
    if let Some(epoch) = epoch {
        if !coherence.coherent_snapshot().is_valid_for_snapshot(epoch) {
            return None;
        }
    }
    let stream = coherence.stream_transactions_until_tip_change(epoch)?;
    Some(stream.map(|item| item.map_err(|_| ServiceError::MempoolStreamLagged)))
}

fn successor(height: Height) -> Result<Height> {
    height
        .checked_add(1)
        .ok_or(ServiceError::HeightOverflow(height))
}
