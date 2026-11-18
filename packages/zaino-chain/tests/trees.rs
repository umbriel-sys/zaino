//! Treestates and subtree roots answered locally (D-51): the store's trees to
//! its tip and the chain head's above it, without asking the validator, and
//! equal to what the validator answers.

mod common;

use std::sync::OnceLock;

use common::{block_at, chain, hash_of, height, rig, Rig, TIP};
use zaino_chain::{BlockId, ChainView as _, ChainViewConfig, TreestateRead as _};
use zaino_primitives::types::ShieldedPool;
use zaino_source_mock::{ChainBuilder, MockBlock, MockChain, NodeFacts};

/// The store's tip: below it the store answers, above it the head.
const STORE_TOP: u32 = TIP - 50;

/// Whether `source` was asked for any of `ports`.
fn asked(source: &MockChain, ports: &[&str]) -> bool {
    source
        .call_log()
        .iter()
        .any(|call| ports.contains(&call.port))
}

#[tokio::test]
async fn treestates_match_the_validator_at_every_height() {
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    // the head without extensions: every treestate is the validator's
    let reference = rig(chain()).store_to(STORE_TOP).build().await;
    let source = chain();
    let snapshot = local
        .view_over(source.clone(), ChainViewConfig::default())
        .snapshot();
    let expected = reference.view().snapshot();

    for h in 0..=TIP {
        let by_height = snapshot
            .treestate(BlockId::Height(height(h)))
            .await
            .expect("answered");
        assert_eq!(
            by_height,
            expected
                .treestate(BlockId::Height(height(h)))
                .await
                .expect("answered"),
            "height {h}"
        );
        assert_eq!(
            snapshot
                .treestate(BlockId::Hash(hash_of(&local.chain, h)))
                .await
                .expect("answered"),
            by_height,
            "hash at {h}"
        );
    }
    assert!(
        !asked(
            &source,
            &[
                "get_treestate",
                "get_treestate_by_hash",
                "get_commitment_tree_roots"
            ]
        ),
        "answered without the validator"
    );
    let at_tip = snapshot
        .treestate(BlockId::Height(height(TIP)))
        .await
        .expect("answered")
        .expect("held");
    assert!(at_tip.sapling.is_some() && at_tip.orchard.is_some());
    assert_eq!(at_tip.block_hash, block_at(&local.chain, TIP).hash());
}

/// Two Sapling subtrees: the first completed below the store's tip, the
/// second above it, in the head's window.
fn subtree_blocks() -> Vec<MockBlock> {
    static BLOCKS: OnceLock<Vec<MockBlock>> = OnceLock::new();
    BLOCKS
        .get_or_init(|| {
            ChainBuilder::new()
                .blocks(3)
                .notes(ShieldedPool::Sapling, 66_000)
                .blocks(5)
                .notes(ShieldedPool::Sapling, 65_536)
                .blocks(5)
                .into_blocks()
        })
        .clone()
}

fn subtree_chain() -> MockChain {
    MockChain::new(NodeFacts::regtest(), subtree_blocks())
}

async fn subtree_rig(extended: bool) -> Rig {
    let shape = rig(subtree_chain()).store_to(45);
    match extended {
        true => shape.extended_head().build().await,
        false => shape.build().await,
    }
}

#[tokio::test]
async fn subtree_roots_join_the_store_and_the_head() {
    let local = subtree_rig(true).await;
    let reference = subtree_rig(false).await;
    let source = subtree_chain();
    let snapshot = local
        .view_over(source.clone(), ChainViewConfig::default())
        .snapshot();
    let expected = reference.view().snapshot();

    let all = snapshot
        .subtree_roots(ShieldedPool::Sapling, 0, None)
        .await
        .expect("answered");
    assert_eq!(all.len(), 2, "one from the store, one from the head");
    assert!(u32::from(all[0].end_height) <= 45 && u32::from(all[1].end_height) > 45);
    for (start, limit) in [
        (0, None),
        (0, Some(1)),
        (0, Some(5)),
        (1, None),
        (1, Some(1)),
        (2, None),
    ] {
        for pool in [ShieldedPool::Sapling, ShieldedPool::Orchard] {
            assert_eq!(
                snapshot
                    .subtree_roots(pool, start, limit)
                    .await
                    .expect("answered"),
                expected
                    .subtree_roots(pool, start, limit)
                    .await
                    .expect("answered"),
                "{pool} from {start}, limit {limit:?}"
            );
        }
    }
    assert!(
        !asked(&source, &["get_subtree_roots"]),
        "answered without the validator"
    );
}

/// The manifest advertises local trees only where both halves build them.
#[tokio::test]
async fn the_manifest_reports_trees_from_both_halves() {
    use zaino_chain::{Answerable, ChainCapability};
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    let mut config = ChainViewConfig::default();
    config.passthrough_enabled = false;
    let manifest = local.view_with(config.clone()).serviceability();
    assert_eq!(
        manifest.get(ChainCapability::Treestate),
        Answerable::ToHeight(height(TIP))
    );
    let plain = rig(chain()).store_to(STORE_TOP).build().await;
    assert_eq!(
        plain
            .view_with(config)
            .serviceability()
            .get(ChainCapability::Treestate),
        Answerable::Absent
    );
}
