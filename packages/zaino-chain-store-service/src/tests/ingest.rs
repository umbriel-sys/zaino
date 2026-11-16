//! The pipeline against an oracle: every index folded one block at a time
//! with each prevout looked up in the whole chain. Whatever the fetch
//! concurrency, queue sizes or writer count, and however a build is split,
//! the store must hold exactly what the oracle does.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use zaino_chain_store::ChainStoreSourceError;
use zaino_indexes::{FoldBlock, IndexId};
use zaino_persistence::{
    in_memory::{InMemoryBackend, InMemoryStorage},
    Backend as _, BackendWriter as _, MaintenanceReader as _, RawKey, RawValue, Storage as _,
};
use zaino_primitives::types::{Height, MerkleRoot, Outpoint, TransparentOutput};
use zaino_source::FailureMode;
use zaino_source_mock::{ChainBuilder, MockChain, NodeFacts};

use super::{boot, chain, config, height, indexes_dir, tip_of};
use crate::assemble::assemble;
use crate::config::{ChainStoreServiceConfig, IngestConfig};
use crate::set::NamedSet;
use crate::store::identity;

/// Every namespace of every index: key → value.
pub(super) type Contents = BTreeMap<(IndexId, &'static str), Vec<(RawKey, RawValue)>>;

pub(super) fn every_index() -> Vec<IndexId> {
    IndexId::ALL.to_vec()
}

fn with(ids: &[IndexId], ingest: IngestConfig) -> ChainStoreServiceConfig {
    ChainStoreServiceConfig {
        ingest,
        ..config(ids)
    }
}

fn ingest(fetch: usize, queue_bytes: usize, writers: usize) -> IngestConfig {
    let n = |n: usize| NonZeroUsize::new(n).expect("non-zero");
    IngestConfig {
        fetch_concurrency: n(fetch),
        queue_bytes: n(queue_bytes),
        writers: n(writers),
    }
}

/// `ids` folded through `tip`, one block at a time, prevouts from the chain.
pub(super) async fn oracle(chain: &MockChain, ids: &[IndexId], tip: Height) -> Contents {
    let trees = ids.contains(&IndexId::TreeState);
    let mut outputs: HashMap<Outpoint, TransparentOutput> = HashMap::new();
    let mut indexes: Vec<_> = ids
        .iter()
        .map(|id| {
            let backend = InMemoryBackend::new(&id.schema().specs()).expect("specs");
            (*id, backend, None)
        })
        .collect();
    for at in 0..=u32::from(tip) {
        let mut parts = assemble(chain, &[], height(at), trees)
            .await
            .expect("assembled");
        let transactions = std::mem::take(&mut parts.transactions);
        for tx in transactions {
            let compact = tx.compact().clone();
            let prevouts = compact
                .transparent_inputs
                .iter()
                .map(|input| {
                    let spent = Outpoint {
                        txid: input.prev_txid,
                        index: input.prev_index,
                    };
                    outputs.get(&spent).cloned().expect("an earlier output")
                })
                .collect();
            for (index, output) in (0u32..).zip(&compact.transparent_outputs) {
                let created = Outpoint {
                    txid: compact.txid,
                    index,
                };
                outputs.insert(created, output.clone());
            }
            parts
                .transactions
                .push(tx.with_prevouts(prevouts).expect("paired"));
        }
        let block = FoldBlock::try_new(parts).expect("a block");
        for (id, backend, state) in &mut indexes {
            let fold = id.fold(state.as_ref(), &block).expect("folds");
            backend
                .writer()
                .expect("writer")
                .commit(block.reference(), fold.ops)
                .expect("commits");
            *state = Some(fold.next);
        }
    }
    indexes
        .iter()
        .flat_map(|(id, backend, _)| contents(*id, backend))
        .collect()
}

fn contents(id: IndexId, backend: &InMemoryBackend) -> Contents {
    let reader = backend.reader().expect("reader");
    id.schema()
        .specs()
        .iter()
        .map(|spec| {
            let rows = reader.scan(spec.namespace).expect("scanned");
            ((id, spec.namespace.as_str()), rows)
        })
        .collect()
}

/// What the store at `/data` holds for `ids`.
pub(super) fn stored(storage: &InMemoryStorage, ids: &[IndexId]) -> Contents {
    ids.iter()
        .flat_map(|id| {
            let dir = indexes_dir().join(id.schema().dir_name());
            let backend = storage
                .open(&dir, identity(*id, "regtest"), &id.schema().specs())
                .expect("the store's own directory");
            contents(*id, &backend)
        })
        .collect()
}

/// Builds `config`'s set over `chain` through `stops`, one `build_to` each.
async fn built(
    config: &ChainStoreServiceConfig,
    chain: &MockChain,
    stops: &[Height],
) -> InMemoryStorage {
    let storage = InMemoryStorage::new();
    let store = boot(config, &storage, chain).await.expect("boots");
    for stop in stops {
        store.build_to(*stop).await.expect("builds");
    }
    storage
}

#[tokio::test(flavor = "multi_thread")]
async fn every_index_matches_the_oracle_however_the_pipeline_is_sized() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let ids = every_index();
    let expected = oracle(&chain, &ids, tip).await;
    for sizes in [
        ingest(1, 1, 1),
        ingest(1, 1 << 30, 1),
        ingest(8, 1, 3),
        ingest(8, 1 << 30, 16),
    ] {
        let storage = built(&with(&ids, sizes.clone()), &chain, &[tip]).await;
        assert_eq!(stored(&storage, &ids), expected, "{sizes:?}");
    }
}

/// A build split in two resolves the first block of the second through
/// I-03 and I-04 (its parent is applied), not the in-flight map.
#[tokio::test(flavor = "multi_thread")]
async fn a_build_resumed_reads_prevouts_from_the_store() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let ids = every_index();
    let expected = oracle(&chain, &ids, tip).await;
    // around the first spends (101), the chained spends (104) and the tip
    let last = u32::from(tip);
    for stop in [1, 100, 101, 102, 103, 104, last - 1] {
        let storage = built(
            &with(&ids, IngestConfig::default()),
            &chain,
            &[height(stop), tip],
        )
        .await;
        assert_eq!(stored(&storage, &ids), expected, "split at {stop}");
    }
}

/// An index enabled after the rest were built folds from genesis, its
/// prevouts read through I-03 and I-04, which skip what they hold.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_enabled_later_catches_up() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let mut lightclient = config(&[]);
    lightclient.indexes.set = Some(NamedSet::Lightclient);
    let storage = InMemoryStorage::new();
    {
        let store = boot(&lightclient, &storage, &chain).await.expect("boots");
        store.build_to(tip).await.expect("builds");
    }
    let mut wider = lightclient.clone();
    wider.indexes.enable = vec![IndexId::AddressHistory.name().to_owned()];
    let store = boot(&wider, &storage, &chain).await.expect("reboots");
    store.build_to(tip).await.expect("catches up");
    let ids: Vec<IndexId> = store.set().iter().collect();
    assert!(ids.contains(&IndexId::AddressHistory));
    assert_eq!(stored(&storage, &ids), oracle(&chain, &ids, tip).await);
}

/// A block whose merkle root does not commit to its transactions is
/// refused at the fetch, whatever the source claims.
#[tokio::test]
async fn a_block_failing_its_merkle_root_is_refused() {
    let mut blocks = ChainBuilder::new().blocks(2).into_blocks();
    if let Some(tampered) = blocks.get_mut(1) {
        tampered.block.header.merkle_root = MerkleRoot::from([9; 32]);
    }
    let chain = MockChain::new(NodeFacts::regtest(), blocks);
    assert!(assemble(&chain, &[], height(0), false).await.is_ok());
    let refused = assemble(&chain, &[], height(1), false).await;
    assert!(
        matches!(&refused, Err(ChainStoreSourceError::InconsistentData { message, .. }) if message.contains("merkle root")),
        "{refused:?}"
    );
}

/// A failed fetch passes nothing past it on; what was fetched before is
/// built and flushed, and a retry finishes the build.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_fetch_builds_everything_before_it() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let ids = every_index();
    let storage = InMemoryStorage::new();
    let store = boot(&with(&ids, ingest(4, 1 << 20, 2)), &storage, &chain)
        .await
        .expect("boots");
    chain.fail_ports(&["get_block"], Some(1), FailureMode::Connection);
    assert!(store.build_to(tip).await.is_err());
    let built = store.subscribe_watermark().borrow().tip;
    assert!(built.is_none_or(|built| built.height < tip), "{built:?}");
    store.build_to(tip).await.expect("retry");
    assert_eq!(stored(&storage, &ids), oracle(&chain, &ids, tip).await);
}
