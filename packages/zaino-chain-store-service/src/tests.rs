//! Boot and build against in-memory storage and the shared fake validator.

use std::path::PathBuf;
use std::sync::Arc;

use zaino_chain_store::StoreCapability;
use zaino_indexes::IndexId;
use zaino_persistence::{in_memory::InMemoryStorage, Identity, SchemaDigest, Storage as _};
use zaino_primitives::types::{BlockRef, Height, Network, ShieldedPool};
use zaino_source::FailureMode;
use zaino_source_mock::{next_block, Activations, BlockSpec, ChainBuilder, MockBlock, MockChain};

use crate::config::{ChainStoreServiceConfig, IndexesConfig, OnCorruption};
use crate::metadata::{self, Metadata};
use crate::set::NamedSet;
use crate::store::{BootError, ChainStore, Serviceability};

/// Every index needing no prevouts.
const PLAIN: [IndexId; 8] = [
    IndexId::Headers,
    IndexId::Txids,
    IndexId::TxidLocation,
    IndexId::Transparent,
    IndexId::ChainMetadata,
    IndexId::Spent,
    IndexId::TreeState,
    IndexId::HistoryTree,
];

fn names(ids: &[IndexId]) -> Vec<String> {
    ids.iter().map(|id| id.name().to_owned()).collect()
}

fn config(enable: &[IndexId]) -> ChainStoreServiceConfig {
    ChainStoreServiceConfig {
        root: PathBuf::from("/data"),
        network: Network::Regtest,
        indexes: IndexesConfig {
            enable: names(enable),
            ..IndexesConfig::default()
        },
        on_corruption: OnCorruption::Rebuild,
    }
}

/// Spends throughout, and notes in both pools.
fn chain() -> MockChain {
    ChainBuilder::new()
        .blocks(3)
        .block(BlockSpec::spends(2).notes(ShieldedPool::Sapling, 5))
        .block(BlockSpec::spends(1).notes(ShieldedPool::Orchard, 3))
        .blocks(2)
        .build()
}

/// The tree-state index builds over a completed subtree (~3 s of real
/// hashing, so only here).
#[tokio::test]
async fn the_tree_state_builds_over_a_completed_subtree() {
    let chain = ChainBuilder::new()
        .blocks(2)
        .notes(ShieldedPool::Sapling, 66_000)
        .blocks(1)
        .build();
    let store = boot(
        &config(&[IndexId::TreeState]),
        &InMemoryStorage::new(),
        &chain,
    )
    .await
    .expect("boots");
    let tip = tip_of(&chain);
    store.build_to(tip.height).await.expect("builds");
    assert_eq!(
        store.serviceability(IndexId::TreeState),
        Serviceability::ToHeight(tip.height)
    );
}

fn height(n: u32) -> Height {
    Height::try_from(n).expect("small")
}

fn tip_of(chain: &MockChain) -> BlockRef {
    let tip = chain.tip().expect("a chain");
    BlockRef {
        hash: tip.hash(),
        height: tip.height(),
    }
}

async fn boot(
    config: &ChainStoreServiceConfig,
    storage: &InMemoryStorage,
    chain: &MockChain,
) -> Result<ChainStore<InMemoryStorage, MockChain>, BootError> {
    ChainStore::boot(
        config,
        storage.clone(),
        Arc::new(chain.clone()),
        Arc::from([]),
    )
    .await
}

#[tokio::test]
async fn the_empty_set_stores_nothing() {
    let store = boot(&config(&[]), &InMemoryStorage::new(), &chain())
        .await
        .expect("boots");
    store.build_to(height(5)).await.expect("nothing to build");
    assert_eq!(store.subscribe_watermark().borrow().tip, None);
    assert_eq!(store.capabilities().iter().count(), 0);
    assert_eq!(
        store.serviceability(IndexId::Headers),
        Serviceability::Absent
    );
}

#[tokio::test]
async fn a_set_builds_to_the_tip_and_reports_what_it_serves() {
    let chain = chain();
    let store = boot(&config(&PLAIN), &InMemoryStorage::new(), &chain)
        .await
        .expect("boots");
    let tip = tip_of(&chain);
    for id in PLAIN {
        assert_eq!(
            store.serviceability(id),
            Serviceability::Building(None),
            "{}",
            id.name()
        );
    }
    store.build_to(tip.height).await.expect("builds");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(tip));
    for id in PLAIN {
        assert_eq!(
            store.serviceability(id),
            Serviceability::ToHeight(tip.height),
            "{}",
            id.name()
        );
    }
    let capabilities = store.capabilities();
    for served in [
        StoreCapability::Core,
        StoreCapability::Transactions,
        StoreCapability::SpentOutputs,
    ] {
        assert!(capabilities.contains(served), "{served}");
    }
    for absent in [
        StoreCapability::CompactBlocks,
        StoreCapability::TxOutSet,
        StoreCapability::TransparentHistory,
        StoreCapability::StoredBlocks,
    ] {
        assert!(!capabilities.contains(absent), "{absent}");
    }
}

#[tokio::test]
async fn a_reboot_resumes_and_continues() {
    let chain = chain();
    let storage = InMemoryStorage::new();
    let first = tip_of(&chain);
    {
        let store = boot(&config(&PLAIN), &storage, &chain)
            .await
            .expect("boots");
        store.build_to(first.height).await.expect("builds");
    }
    let parent = chain.tip().expect("a chain");
    chain.extend(next_block(
        Some(&parent),
        &Activations::default(),
        BlockSpec::spends(2),
    ));
    let store = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("reboots");
    assert_eq!(
        store.subscribe_watermark().borrow().tip,
        Some(first),
        "resumed"
    );
    let next = tip_of(&chain);
    store.build_to(next.height).await.expect("continues");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(next));
}

#[tokio::test]
async fn a_set_needing_prevouts_waits_for_the_resolver() {
    let mut lightclient = config(&[]);
    lightclient.indexes.set = Some(NamedSet::Lightclient);
    let refused = boot(&lightclient, &InMemoryStorage::new(), &chain())
        .await
        .err();
    assert!(
        matches!(refused, Some(BootError::PrevoutsUnsupported(needing)) if needing.contains("compact_block"))
    );
}

#[tokio::test]
async fn one_store_per_directory() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let _held = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("boots");
    let refused = boot(&config(&PLAIN), &storage, &chain).await.err();
    assert!(matches!(
        refused,
        Some(BootError::Storage { index: "store", .. })
    ));
}

fn indexes_dir() -> PathBuf {
    PathBuf::from("/data/regtest/indexes")
}

fn foreign(storage: &InMemoryStorage, name: &str) {
    let identity = Identity {
        name: "foreign",
        chain: "regtest",
        format: 1,
        schema: SchemaDigest::new([9; 32]),
    };
    storage
        .open(
            &indexes_dir().join(name),
            identity,
            &IndexId::Txids.schema().specs(),
        )
        .expect("created");
}

#[tokio::test]
async fn stale_directories_go_unless_kept_and_foreign_ones_stay() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    drop(
        boot(
            &config(&[IndexId::Headers, IndexId::Txids]),
            &storage,
            &chain,
        )
        .await
        .expect("boots"),
    );
    foreign(&storage, "headers_0");
    foreign(&storage, "notes");
    let headers = IndexId::Headers.schema().dir_name();
    let txids = IndexId::Txids.schema().dir_name();

    let mut keep = config(&[IndexId::Headers]);
    keep.indexes.delete_stale = false;
    drop(boot(&keep, &storage, &chain).await.expect("boots"));
    let mut kept = storage.children(&indexes_dir()).expect("listed");
    kept.sort();
    assert_eq!(kept, {
        let mut all = vec![
            "headers_0".to_owned(),
            headers.clone(),
            "notes".to_owned(),
            txids.clone(),
        ];
        all.sort();
        all
    });

    drop(
        boot(&config(&[IndexId::Headers]), &storage, &chain)
            .await
            .expect("boots"),
    );
    let mut left = storage.children(&indexes_dir()).expect("listed");
    left.sort();
    assert_eq!(
        left,
        [headers, "notes".to_owned()],
        "old format and disabled deleted, foreign left"
    );
}

#[tokio::test]
async fn a_tip_the_validator_does_not_have_is_rebuilt() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    {
        let store = boot(&config(&PLAIN), &storage, &chain)
            .await
            .expect("boots");
        store.build_to(tip_of(&chain).height).await.expect("builds");
    }
    // a different chain of the same height: the stored tip's height, another block
    reorg_from_4(&chain, 7);
    let store = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("reboots");
    assert_eq!(
        store.subscribe_watermark().borrow().tip,
        None,
        "every index rebuilt"
    );
    let tip = tip_of(&chain);
    store
        .build_to(tip.height)
        .await
        .expect("rebuilds on the new chain");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(tip));
}

#[tokio::test]
async fn another_layout_in_the_directory_is_rebuilt() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let dir = IndexId::Headers.schema().dir_name();
    foreign(&storage, &dir);
    let store = boot(&config(&[IndexId::Headers]), &storage, &chain)
        .await
        .expect("boots");
    store.build_to(height(2)).await.expect("builds");
    assert_eq!(
        store.serviceability(IndexId::Headers),
        Serviceability::ToHeight(height(2))
    );
}

#[tokio::test]
async fn boot_records_the_set() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    drop(
        boot(&config(&[IndexId::Spent]), &storage, &chain)
            .await
            .expect("boots"),
    );
    let bytes = storage
        .read_meta(&PathBuf::from("/data/regtest"), metadata::NAME)
        .expect("read")
        .expect("written");
    assert_eq!(
        Metadata::decode(&bytes),
        Ok(Metadata::of("regtest", [IndexId::Txids, IndexId::Spent]))
    );
}

#[tokio::test]
async fn concurrent_builds_are_one_build() {
    let chain = chain();
    let store = boot(&config(&PLAIN), &InMemoryStorage::new(), &chain)
        .await
        .expect("boots");
    let tip = tip_of(&chain);
    let (a, b) = tokio::join!(store.build_to(tip.height), store.build_to(tip.height));
    a.expect("first");
    b.expect("second");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(tip));
    assert_eq!(
        chain.calls("get_block"),
        usize::try_from(u32::from(tip.height)).expect("small heights") + 1,
        "each block fetched once"
    );
}

#[tokio::test]
async fn a_failed_fetch_leaves_the_store_consistent_and_a_retry_finishes() {
    let chain = chain();
    let store = boot(&config(&PLAIN), &InMemoryStorage::new(), &chain)
        .await
        .expect("boots");
    let tip = tip_of(&chain);
    store.build_to(height(2)).await.expect("builds");
    chain.fail_ports(&["get_block"], Some(1), FailureMode::Connection);
    assert!(store.build_to(tip.height).await.is_err());
    let after = store.subscribe_watermark().borrow().tip;
    assert_eq!(
        after.map(|tip| tip.height),
        Some(height(2)),
        "nothing past the failure"
    );
    store.build_to(tip.height).await.expect("retry");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(tip));
}

/// Replaces the served chain from height 4 with a `seed` branch reaching the
/// same tip height.
fn reorg_from_4(chain: &MockChain, seed: u64) {
    let top = u32::from(chain.tip().expect("loaded").height());
    let mut branch: Vec<MockBlock> = Vec::new();
    for _ in 4..=top {
        let parent = branch.last().cloned().or_else(|| chain.block_at(3));
        branch.push(next_block(
            parent.as_ref(),
            &Activations::default(),
            BlockSpec::spends(1).seeded(seed),
        ));
    }
    chain.reorg(4, branch);
}

/// A store built past the validator's tip boots, holds those indexes back
/// (not served), and checks them once the validator reaches them.
async fn built_then_behind() -> (InMemoryStorage, MockChain, BlockRef) {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let tip = tip_of(&chain);
    let store = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("boots");
    store.build_to(tip.height).await.expect("builds");
    (storage, chain, tip)
}

#[tokio::test]
async fn a_validator_behind_holds_the_indexes_until_it_catches_up() {
    let (storage, chain, tip) = built_then_behind().await;
    chain.serve_to(4);
    let store = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("boots behind");
    assert_eq!(
        store.subscribe_watermark().borrow().tip,
        None,
        "held, not served"
    );
    assert_eq!(
        store.serviceability(IndexId::Headers),
        Serviceability::Building(None)
    );
    assert_eq!(store.capabilities().iter().count(), 0);
    store
        .build_to(height(4))
        .await
        .expect("nothing to build yet");
    assert_eq!(store.subscribe_watermark().borrow().tip, None, "still held");
    chain.mine(u32::MAX);
    let fetched = chain.calls("get_block");
    store
        .build_to(tip.height)
        .await
        .expect("checks and resumes");
    assert_eq!(
        store.subscribe_watermark().borrow().tip,
        Some(tip),
        "verified, not rebuilt"
    );
    assert_eq!(
        chain.calls("get_block") - fetched,
        1,
        "one block for the check, none rebuilt"
    );
}

#[tokio::test]
async fn a_validator_behind_on_another_chain_rebuilds_once_it_catches_up() {
    let (storage, chain, _) = built_then_behind().await;
    reorg_from_4(&chain, 5);
    chain.serve_to(4);
    let store = boot(&config(&PLAIN), &storage, &chain)
        .await
        .expect("boots behind");
    chain.mine(u32::MAX);
    let tip = tip_of(&chain);
    store
        .build_to(tip.height)
        .await
        .expect("rebuilds on the validator's chain");
    assert_eq!(store.subscribe_watermark().borrow().tip, Some(tip));
}
