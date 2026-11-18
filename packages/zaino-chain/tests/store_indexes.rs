//! The store seeding the chain head's extensions (D-50): `StoreIndexes`
//! answers the five ports from the real store, and a head extended over it
//! agrees with a store built over the whole chain.

mod common;

use std::sync::Arc;

use common::{
    block_at, chain, eventually, hash_of, height, ledger, rig, without_indexes, Rig, TIP,
};
use tokio_util::sync::CancellationToken;
use zaino_chain::StoreIndexes;
use zaino_chain_head::extension::{
    HistoryTreeSeed as _, PrevoutSource as _, RemainingOutputs as _, SeedError, TreeStateSeed as _,
    TxOutSetSeed as _,
};
use zaino_chain_head::snapshot::{
    ChainHeadHistoryTreeService as _, ChainHeadTransactionService as _,
    ChainHeadTransparentHistoryService as _, ChainHeadTreeStateService as _,
    ChainHeadTxOutSetService as _,
};
use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadSnapshot as _, HeadCapability};
use zaino_chain_store::{
    ChainStoreService as _, HistoryTreeIndex as _, SpentOutputIndex as _,
    TransparentHistoryIndex as _, TreeStateIndex as _, TxOutSetIndex as _,
};
use zaino_primitives::types::{BlockHash, BlockRef, Outpoint, ShieldedPool, TransparentAddressKey};
use zaino_source_mock::{MockChain, MINER_SCRIPT, RECIPIENT_SCRIPT};

/// The store's watermark, the head's root.
const ROOT: u32 = TIP - 50;

fn block_ref(chain: &MockChain, h: u32) -> BlockRef {
    BlockRef {
        hash: hash_of(chain, h),
        height: height(h),
    }
}

async fn store_to(chain: &MockChain, top: u32) -> Rig {
    rig(chain.clone()).store_to(top).build().await
}

/// The extensions the head's snapshot has not built, with why.
fn dark(rig: &Rig) -> Vec<(HeadCapability, String)> {
    rig.head.subscriber().current().extension_failures()
}

/// Waits until the sync has built the store to the head's floor.
async fn store_at_floor(rig: &Rig) {
    assert!(
        eventually(|| rig.store_top() == Some(rig.head_floor())).await,
        "the sync builds the store to the floor"
    );
}

/// With the store empty only the index needing nothing below is built; once
/// the sync builds the store to the floor, the next tick builds the rest
/// though no block arrived.
#[tokio::test]
async fn the_indexes_build_when_the_store_moves_under_an_unchanged_head() {
    let rig = rig(chain()).extended_head().build().await;
    let failed: Vec<HeadCapability> = dark(&rig).into_iter().map(|(c, _)| c).collect();
    assert_eq!(
        failed,
        vec![
            HeadCapability::TxOutSet,
            HeadCapability::TransparentHistory,
            HeadCapability::TreeState,
            HeadCapability::HistoryTree
        ]
    );
    let sync = Arc::new(rig.view()).spawn_sync(CancellationToken::new());
    store_at_floor(&rig).await;
    rig.advance().await;
    assert!(dark(&rig).is_empty(), "{:?}", dark(&rig));
    sync.shutdown();
}

/// Production's shape: the sync builds the store to the head's floor, so
/// when a block arrives the store's tip is the new floor's parent. The
/// indexes start there and stay built block by block.
#[tokio::test]
async fn under_chain_view_sync_the_indexes_stay_built_block_by_block() {
    let rig = rig(chain()).extended_head().build().await;
    let sync = Arc::new(rig.view()).spawn_sync(CancellationToken::new());
    // the head grows its window by a margin before its floor moves
    let mut floor_moved = 0;
    for _ in 0..16 {
        store_at_floor(&rig).await;
        let store_tip = rig.store_top();
        let floor = rig.head_floor();
        rig.extend(1);
        rig.advance().await;
        if rig.head_floor() > floor {
            floor_moved += 1;
            assert_eq!(
                store_tip.and_then(|tip| tip.checked_add(1)),
                Some(rig.head_floor()),
                "the store's tip is the floor's parent"
            );
        }
        assert!(dark(&rig).is_empty(), "{:?}", dark(&rig));
    }
    assert!(floor_moved >= 4, "the floor moved {floor_moved} times");
    sync.shutdown();
}

#[tokio::test]
async fn the_seeds_are_the_stores_facts_at_the_root() {
    let chain = chain();
    let store = store_to(&chain, ROOT).await.store;
    let reader = store.reader();
    let indexes = StoreIndexes::new(store.clone());
    let root = block_ref(&chain, ROOT);

    assert_eq!(
        indexes.accumulator_at(root).await.expect("seeded"),
        reader.txout_set().await.expect("read")
    );
    let frontiers = indexes.frontiers_at(root).await.expect("seeded");
    for (frontier, pool) in frontiers.iter().zip(ShieldedPool::ALL) {
        assert_eq!(
            Some(frontier),
            reader
                .pool_frontier(pool, root.height)
                .await
                .expect("read")
                .as_ref(),
            "{pool}"
        );
    }
    let epoch = indexes
        .epoch_at(root)
        .await
        .expect("seeded")
        .expect("Heartwood is active");
    let stored = reader
        .history_epoch(root.height)
        .await
        .expect("read")
        .expect("Heartwood is active");
    assert_eq!((epoch.branch, epoch.length), (stored.branch, stored.length));
    assert_eq!(
        zaino_indexes::history_tree::root_of(&zaino_indexes::history_tree::Epoch {
            branch: u32::from(epoch.branch),
            start: 0,
            length: epoch.length,
            peaks: epoch.peaks,
        })
        .expect("peaks decode"),
        reader
            .history_root(root.height)
            .await
            .expect("read")
            .expect("Heartwood is active")
    );

    let (created, _) = ledger(&chain, ROOT);
    let outpoints: Vec<Outpoint> = created.iter().rev().take(8).copied().collect();
    assert_eq!(
        indexes
            .previous_outputs(root, &outpoints)
            .await
            .expect("seeded"),
        reader.outputs_by_outpoint(&outpoints).await.expect("read")
    );
    assert!(indexes
        .previous_outputs(root, &outpoints)
        .await
        .expect("seeded")
        .iter()
        .all(Option::is_some));
    let txids: Vec<_> = outpoints.iter().map(|outpoint| outpoint.txid).collect();
    assert_eq!(
        indexes
            .remaining_outputs(root, &txids)
            .await
            .expect("seeded"),
        reader.remaining_outputs(&txids).await.expect("read")
    );
}

#[tokio::test]
async fn a_root_the_store_does_not_hold_is_refused() {
    let chain = chain();
    let indexes = StoreIndexes::new(store_to(&chain, ROOT).await.store);
    let wrong_hash = BlockRef {
        hash: BlockHash::from([0x5a; 32]),
        height: height(ROOT),
    };
    assert!(matches!(
        indexes.accumulator_at(wrong_hash).await,
        Err(SeedError::RootNotHeld { root }) if root == wrong_hash
    ));
    assert!(matches!(
        indexes.frontiers_at(wrong_hash).await,
        Err(SeedError::RootNotHeld { .. })
    ));
    assert!(matches!(
        indexes.epoch_at(wrong_hash).await,
        Err(SeedError::RootNotHeld { .. })
    ));
    assert!(matches!(
        indexes.previous_outputs(wrong_hash, &[]).await,
        Err(SeedError::RootNotHeld { .. })
    ));
    // above the watermark: not held either
    assert!(indexes
        .remaining_outputs(block_ref(&chain, TIP), &[])
        .await
        .is_err());
}

/// The accumulator and the remaining counts exist only at the watermark; the
/// height-indexed seeds answer at any held height.
#[tokio::test]
async fn the_accumulator_is_the_watermarks_alone() {
    let chain = chain();
    let indexes = StoreIndexes::new(store_to(&chain, ROOT).await.store);
    let below = block_ref(&chain, ROOT - 1);
    assert!(matches!(
        indexes.accumulator_at(below).await,
        Err(SeedError::Unavailable { .. })
    ));
    assert!(matches!(
        indexes.remaining_outputs(below, &[]).await,
        Err(SeedError::Unavailable { .. })
    ));
    assert!(indexes.frontiers_at(below).await.is_ok());
    assert!(indexes.epoch_at(below).await.is_ok());
    assert!(indexes.previous_outputs(below, &[]).await.is_ok());
}

#[tokio::test]
async fn a_store_without_the_index_is_unavailable() {
    let chain = chain();
    let store = rig(chain.clone())
        .indexes(without_indexes(&["tx_out_set", "history_tree"]))
        .store_to(ROOT)
        .build()
        .await
        .store;
    let indexes = StoreIndexes::new(store);
    let root = block_ref(&chain, ROOT);
    assert!(matches!(
        indexes.accumulator_at(root).await,
        Err(SeedError::Unavailable { .. })
    ));
    assert!(matches!(
        indexes.epoch_at(root).await,
        Err(SeedError::Unavailable { .. })
    ));
    assert!(indexes.frontiers_at(root).await.is_ok());
}

/// Everything the extended head answers above the root equals a store built
/// over the whole chain, before and after the chain grows and the root moves.
#[tokio::test]
async fn an_extended_head_agrees_with_a_store_over_the_whole_chain() {
    let chain = chain();
    let rig = rig(chain.clone())
        .store_to(ROOT)
        .extended_head()
        .build()
        .await;
    let reference = store_to(&chain, TIP).await.store;
    agree(&rig, &reference, ROOT, TIP).await;

    // the chain grows; the store follows and the root moves past a rebase
    rig.extend(3);
    rig.store.build_to(height(ROOT + 40)).await.expect("builds");
    rig.advance().await;
    reference.build_to(height(TIP + 3)).await.expect("builds");
    agree(&rig, &reference, ROOT + 40, TIP + 3).await;
}

/// The head's answers over `root+1..=tip` against `reference`, a store built
/// through `tip`.
async fn agree(rig: &Rig, reference: &common::Store, root: u32, tip: u32) {
    let chain = &rig.chain;
    let snapshot = rig.head.subscriber().current();
    let reference = reference.reader();
    let capabilities = snapshot.capabilities();
    assert!(
        HeadCapability::ALL
            .iter()
            .all(|capability| capabilities.contains(*capability)),
        "{capabilities:?}"
    );

    assert_eq!(
        snapshot.txout_set().expect("built"),
        reference.txout_set().await.expect("read")
    );

    let seed_length = reference
        .history_epoch(height(root))
        .await
        .expect("read")
        .expect("Heartwood is active")
        .length;
    for h in root + 1..=tip {
        let at = height(h);
        assert_eq!(
            snapshot.history_root(at),
            reference.history_root(at).await.expect("read"),
            "history root at {h}"
        );
        let epoch = reference
            .history_epoch(at)
            .await
            .expect("read")
            .expect("Heartwood is active");
        let head_epoch = snapshot.history_epoch(at).expect("an epoch");
        assert_eq!(
            (head_epoch.branch, head_epoch.length),
            (epoch.branch, epoch.length)
        );
        // positions the window appended are the head's, the rest the store's
        let positions = [seed_length - 1, seed_length, epoch.length - 1];
        let head_entries = snapshot.history_entries(at, &positions);
        let stored = reference
            .history_entries(at, &positions)
            .await
            .expect("read");
        assert_eq!(head_entries[0], None, "below the seed at {h}");
        assert_eq!(head_entries[1], stored[1], "first appended at {h}");
        assert_eq!(head_entries[2], stored[2], "last appended at {h}");
        for pool in ShieldedPool::ALL {
            assert_eq!(
                snapshot.pool_frontier(pool, at),
                reference.pool_frontier(pool, at).await.expect("read"),
                "{pool} at {h}"
            );
        }
        for (tx_index, tx) in (0u32..).zip(block_at(chain, h).block.transactions()) {
            let best = snapshot
                .transaction_locations(&tx.txid)
                .best_chain
                .expect("located");
            assert_eq!((best.block, best.tx_index), (block_ref(chain, h), tx_index));
        }
    }
    assert!(snapshot.history_root(height(root)).is_none());
    assert!(snapshot
        .pool_frontier(ShieldedPool::Sapling, height(root))
        .is_none());

    // spenders: every outpoint the window spent, and one spent below the root
    let (created, spenders) = ledger(chain, tip);
    let (_, spent_below) = ledger(chain, root);
    let mut outpoints: Vec<Outpoint> = spenders
        .keys()
        .filter(|outpoint| !spent_below.contains_key(outpoint))
        .copied()
        .collect();
    assert!(!outpoints.is_empty(), "the window spends");
    let below = *spent_below.keys().next().expect("spends below the root");
    outpoints.push(below);
    for (outpoint, spender) in outpoints.iter().zip(snapshot.outpoint_spenders(&outpoints)) {
        match *outpoint == below {
            true => assert!(spender.is_none(), "below the root is the store's"),
            false => assert_eq!(
                spender.expect("spent in the window").txid,
                spenders[outpoint]
            ),
        }
    }
    assert!(created.contains(&below));

    // address effects over the window, per address the chain uses
    for script in [MINER_SCRIPT.as_slice(), RECIPIENT_SCRIPT.as_slice()] {
        let address = TransparentAddressKey::from_standard_script(script).expect("standard");
        let query = zaino_chain_head::transparent::TransparentHistoryQuery {
            addresses: vec![address],
            start: height(root + 1),
            end: height(tip),
        };
        let effects = snapshot.address_effects(&query).expect("built");
        let stored = reference
            .address_effects(&zaino_chain_store::TransparentHistoryQuery {
                addresses: vec![address],
                start: height(root + 1),
                end: height(tip),
            })
            .await
            .expect("read");
        let mut outputs: Vec<Outpoint> = effects.outputs.iter().map(|o| o.outpoint).collect();
        let mut expected: Vec<Outpoint> = stored.outputs.iter().map(|o| o.outpoint).collect();
        outputs.sort_unstable_by_key(|o| (<[u8; 32]>::from(o.txid), o.index));
        expected.sort_unstable_by_key(|o| (<[u8; 32]>::from(o.txid), o.index));
        assert_eq!(outputs, expected, "outputs of {address:?}");
        let mut spends: Vec<Outpoint> = effects.local_spends.iter().map(|s| s.outpoint).collect();
        let mut expected: Vec<Outpoint> = stored.spends.iter().map(|s| s.outpoint).collect();
        spends.sort_unstable_by_key(|o| (<[u8; 32]>::from(o.txid), o.index));
        expected.sort_unstable_by_key(|o| (<[u8; 32]>::from(o.txid), o.index));
        assert_eq!(spends, expected, "spends of {address:?}");
        assert!(
            !outputs.is_empty() || !spends.is_empty(),
            "{address:?} is used"
        );
    }

    let _ = Arc::clone(&snapshot);
}
