//! The build pipeline (D-67, D-96): fetch → resolve → one writer per index,
//! all at once, so the slowest stage alone sets the pace.
//!
//! - fetch: up to `fetch_concurrency` heights at once, each in its own task,
//!   taken back in height order;
//! - resolve: in height order, prevouts only where an index needs them;
//! - writers: one per index on a blocking thread, behind a byte-bounded
//!   queue; a block goes on once every queue that wants it has room.
//!
//! On any failure nothing more is fetched; each writer finishes what it was
//! given, flushes and hands its index back, and the first failure is
//! returned. A panic in any stage is caught and returned as a failure; an
//! index whose writer panicked comes back marked to be reopened.

mod queue;
mod resolve;
mod writer;

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};

use futures::{FutureExt as _, StreamExt as _};
use tokio::sync::{watch, Semaphore};
use zaino_chain_store::{ChainStoreSource, ChainStoreSourceError};
use zaino_indexes::{FoldBlock, FoldBlockParts, FoldTx, IndexId};
use zaino_persistence::Backend;
use zaino_primitives::types::{BlockRef, Height, NetworkUpgradeInfo};

use crate::assemble::{assemble, inconsistent};
use crate::config::IngestConfig;
use crate::store::Open;
use resolve::{Resolver, Source};

/// What the build fetches from, and how hard.
pub(crate) struct Ingest<'a, Src> {
    pub(crate) source: &'a Arc<Src>,
    pub(crate) schedule: &'a Arc<[NetworkUpgradeInfo]>,
    /// Fetch frontiers and subtrees (C2 is built).
    pub(crate) trees: bool,
    pub(crate) config: &'a IngestConfig,
}

/// An index being built: how far its writer has applied, and its backend.
pub(crate) struct Running<B> {
    pub(crate) id: IndexId,
    pub(crate) backend: Arc<B>,
    pub(crate) applied: watch::Receiver<Option<BlockRef>>,
}

/// An index back from a build: `panicked` = its writer panicked, so it must
/// be reopened from disk.
pub(crate) struct Returned<B: Backend> {
    pub(crate) open: Open<B>,
    pub(crate) panicked: bool,
}

/// Builds `indexes` from the height after the lowest tip through `target`,
/// calling `progress` after each block is passed on. Returns every index,
/// in order, with the first failure; a panic anywhere is caught and
/// returned as a failure.
pub(crate) async fn build<B, Src>(
    indexes: Vec<Open<B>>,
    target: Height,
    ingest: Ingest<'_, Src>,
    mut progress: impl FnMut(&[Running<B>]),
) -> (Vec<Returned<B>>, Result<(), ChainStoreSourceError>)
where
    B: Backend + 'static,
    B::Writer: 'static,
    Src: ChainStoreSource,
{
    let unchanged = |indexes: Vec<Open<B>>| {
        let returned = indexes.into_iter().map(|open| Returned {
            open,
            panicked: false,
        });
        (returned.collect(), Ok(()))
    };
    let start = indexes.iter().map(|index| next_height(index.tip)).min();
    let Some(Some(start)) = start.filter(|_| !indexes.is_empty()) else {
        return unchanged(indexes);
    };
    if start > target {
        return unchanged(indexes);
    }

    let writers = Arc::new(Semaphore::new(ingest.config.writers.get()));
    let runtime = tokio::runtime::Handle::current();
    let mut running = Vec::with_capacity(indexes.len());
    let mut lanes = Vec::with_capacity(indexes.len());
    let mut slots = Vec::with_capacity(indexes.len());
    let mut tasks = Vec::with_capacity(indexes.len());
    // where each index's prevouts are first needed (`None`: never)
    let mut needed: Option<Height> = None;
    for open in indexes {
        let (sender, receiver) = queue::queue(ingest.config.queue_bytes);
        let (applied, watched) = watch::channel(open.tip);
        if open.id.builds_from().next().is_some() {
            let from = next_height(open.tip);
            needed = match (needed, from) {
                (Some(low), Some(from)) => Some(low.min(from)),
                (low, from) => low.or(from),
            };
        }
        running.push(Running {
            id: open.id,
            backend: Arc::clone(&open.backend),
            applied: watched,
        });
        lanes.push((open.tip, sender));
        let slot: writer::Slot<B> = Arc::new(Mutex::new(Some(open)));
        let (writers, runtime, held) = (Arc::clone(&writers), runtime.clone(), Arc::clone(&slot));
        slots.push(slot);
        tasks.push(tokio::task::spawn_blocking(move || {
            writer::write(&held, receiver, applied, writers, runtime)
        }));
    }
    let mut resolver = needed.and_then(|_| {
        let source = |id: IndexId| {
            running
                .iter()
                .find(|index| index.id == id)
                .map(|index| Source {
                    backend: Arc::clone(&index.backend),
                    applied: index.applied.clone(),
                })
        };
        Some(Resolver::new(
            source(IndexId::TxidLocation)?,
            source(IndexId::Transparent)?,
        ))
    });

    let result = AssertUnwindSafe(async {
        if needed.is_some() && resolver.is_none() {
            return Err(ChainStoreSourceError::inconsistent_data(
                "an index needs prevouts, but txid_location or transparent is not building",
            ));
        }
        let mut fetched = futures::stream::iter(u32::from(start)..=u32::from(target))
            .map(|height| {
                let (source, schedule) = (Arc::clone(ingest.source), Arc::clone(ingest.schedule));
                let trees = ingest.trees;
                tokio::spawn(async move {
                    let height = Height::try_from(height).map_err(inconsistent)?;
                    assemble(source.as_ref(), &schedule, height, trees).await
                })
            })
            .buffered(ingest.config.fetch_concurrency.get());
        while let Some(parts) = fetched.next().await {
            let parts = parts.map_err(|error| joined("a fetch task", error))??;
            let height = parts.header.height;
            let block = match resolver.take() {
                Some(mut taken) => {
                    let wants = needed.is_some_and(|from| height >= from);
                    let (back, resolved) = tokio::task::spawn_blocking(move || {
                        let resolved = taken.resolve(parts, wants);
                        (taken, resolved)
                    })
                    .await
                    .map_err(|error| joined("prevout resolution", error))?;
                    resolver = Some(back);
                    finish(resolved?)?
                }
                None => finish(parts)?,
            };
            let block = Arc::new(block);
            let bytes = weight(&block);
            for (tip, lane) in &lanes {
                let wanted = tip.is_none_or(|tip| tip.height < height);
                // a writer stopped: its failure is the one returned
                if wanted && lane.send(Arc::clone(&block), bytes).await.is_err() {
                    return Ok(());
                }
            }
            progress(&running);
        }
        Ok(())
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| {
        let message = writer::panic_message(&*payload);
        tracing::error!(%message, "the build pipeline panicked");
        Err(writer::panicked("the build pipeline", &message))
    });

    drop(lanes);
    let mut failure = result.err();
    let mut panicked = Vec::with_capacity(tasks.len());
    for task in tasks {
        let written = task.await.unwrap_or_else(|error| writer::Written {
            result: Err(joined("an index writer", error)),
            panicked: true,
        });
        if let Err(error) = written.result {
            failure.get_or_insert(error);
        }
        panicked.push(written.panicked);
    }
    // every writer has ended; each index is taken back from its slot
    let mut returned = Vec::with_capacity(slots.len());
    for (slot, panicked) in slots.into_iter().zip(panicked) {
        let taken = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(open) = taken {
            returned.push(Returned { open, panicked });
        }
    }
    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| progress(&running))) {
        let message = writer::panic_message(&*payload);
        failure.get_or_insert(writer::panicked("publishing progress", &message));
    }
    (returned, failure.map_or(Ok(()), Err))
}

/// A task that did not finish: its panic, or its cancellation.
fn joined(what: &str, error: tokio::task::JoinError) -> ChainStoreSourceError {
    match error.try_into_panic() {
        Ok(payload) => {
            let message = writer::panic_message(&*payload);
            tracing::error!(%message, "{what} panicked");
            writer::panicked(what, &message)
        }
        Err(error) => ChainStoreSourceError::Commit {
            message: format!("{what} was cancelled"),
            cause: Some(Box::new(error)),
        },
    }
}

fn finish(parts: FoldBlockParts) -> Result<FoldBlock, ChainStoreSourceError> {
    FoldBlock::try_new(parts).map_err(inconsistent)
}

/// The height after `tip`; genesis for an empty index; `None` past the last.
pub(crate) fn next_height(tip: Option<BlockRef>) -> Option<Height> {
    match tip {
        None => Some(Height::GENESIS),
        Some(tip) => tip.height.checked_add(1),
    }
}

/// Header bytes a block holds in memory, roughly (with its solution).
const HEADER_BYTES: usize = 1_600;
/// Bytes per transaction beyond its parts: its txid, balances, list heads.
const TX_BYTES: usize = 128;
/// Bytes per transparent input (outpoint), Sapling nullifier, Sapling output
/// and Orchard-shaped action.
const INPUT_BYTES: usize = 36;
const NULLIFIER_BYTES: usize = 32;
const SAPLING_OUTPUT_BYTES: usize = 116;
const ACTION_BYTES: usize = 148;

/// What a block holds in memory, estimated: the unit of a queue's budget.
fn weight(block: &FoldBlock) -> usize {
    let transactions = block.transactions().iter().map(tx_weight);
    transactions.fold(HEADER_BYTES, usize::saturating_add)
}

fn tx_weight(tx: &FoldTx) -> usize {
    let compact = tx.compact();
    let outputs = |outputs: &[zaino_primitives::types::TransparentOutput]| {
        outputs
            .iter()
            .map(|output| output.script.as_bytes().len().saturating_add(8))
            .fold(0usize, usize::saturating_add)
    };
    let counted = |count: usize, bytes: usize| count.saturating_mul(bytes);
    [
        TX_BYTES,
        counted(compact.transparent_inputs.len(), INPUT_BYTES),
        outputs(&compact.transparent_outputs),
        tx.prevouts().map_or(0, outputs),
        counted(compact.sapling_nullifiers.len(), NULLIFIER_BYTES),
        counted(compact.sapling_outputs.len(), SAPLING_OUTPUT_BYTES),
        counted(
            compact
                .orchard_actions
                .len()
                .saturating_add(compact.ironwood_actions.len()),
            ACTION_BYTES,
        ),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    /// A fetch or resolver task's panic becomes an error naming the task and
    /// the panic.
    #[tokio::test]
    async fn a_task_panic_becomes_an_error_naming_it() {
        let error = tokio::spawn(async { panic!("boom") })
            .await
            .expect_err("the task panicked");
        let mapped = super::joined("a fetch task", error);
        assert!(
            mapped.to_string().contains("a fetch task panicked: boom"),
            "{mapped}"
        );
    }
}
