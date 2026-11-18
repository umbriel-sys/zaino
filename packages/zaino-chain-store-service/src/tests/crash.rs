//! A build cut by a failed commit, which is what a crash amounts to at the
//! port: a commit that returned is kept, one that failed is not (file
//! durability is the engine's contract, tested there). The store reopens
//! with every index at its own tip, serves the lowest, resumes each from its
//! own, and keeps going in the same process once the disk works again.

use std::collections::BTreeMap;

use zaino_indexes::IndexId;
use zaino_primitives::types::Height;
use zaino_source_mock::MockChain;

use super::ingest::{every_index, oracle, stored, Contents};
use super::panics::{boot, Faulty};
use super::{chain, height, tip_of};
use crate::store::{ChainStore, Serviceability};

/// Each index's built height as the store reports it.
fn tips(store: &ChainStore<Faulty, MockChain>) -> BTreeMap<IndexId, Option<Height>> {
    every_index()
        .into_iter()
        .map(|id| {
            let tip = match store.serviceability(id) {
                Serviceability::ToHeight(tip) => Some(tip),
                Serviceability::Building(tip) => tip,
                Serviceability::Absent => panic!("{} is enabled", id.name()),
            };
            (id, tip)
        })
        .collect()
}

/// Every index holds exactly the oracle at its own tip.
async fn assert_each_at_its_tip(
    chain: &MockChain,
    storage: &Faulty,
    tips: &BTreeMap<IndexId, Option<Height>>,
    label: &str,
) {
    let ids = every_index();
    let mut oracles: BTreeMap<Height, Contents> = BTreeMap::new();
    for ((id, namespace), rows) in &stored(&storage.inner, &ids) {
        let Some(tip) = tips[id] else {
            assert!(rows.is_empty(), "{label}: {}/{namespace}", id.name());
            continue;
        };
        let expected = match oracles.entry(tip) {
            std::collections::btree_map::Entry::Occupied(held) => held.into_mut(),
            std::collections::btree_map::Entry::Vacant(empty) => {
                empty.insert(oracle(chain, &ids, tip).await)
            }
        };
        assert_eq!(
            rows,
            &expected[&(*id, *namespace)],
            "{label}: {}/{namespace} at {tip}",
            id.name()
        );
    }
}

/// The height whose commit fails: the chain's spends block, so every index
/// has rows before and after the cut.
fn cut(tip: Height) -> u32 {
    u32::from(tip) - 2
}

#[tokio::test]
async fn a_build_cut_by_a_failed_commit_reopens_each_index_at_its_own_tip_and_resumes() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let cut = cut(tip);
    let ids = every_index();
    let expected = oracle(&chain, &ids, tip).await;
    for failing in ids {
        let label = failing.name();
        let storage = Faulty::default();
        let store = boot(&storage, &chain).await;
        storage.fail_commit_after(failing, cut);
        let failed = store.build_to(tip).await.expect_err("the commit failed");
        let message = failed.to_string();
        assert!(
            message.contains(label) && !message.contains("panicked"),
            "{message}"
        );
        drop(store);

        let store = boot(&storage, &chain).await;
        let tips = tips(&store);
        let before_cut = height(cut - 1);
        assert_eq!(tips[&failing], Some(before_cut), "{label}");
        for (id, index_tip) in &tips {
            assert!(
                index_tip.is_some_and(|index_tip| before_cut <= index_tip && index_tip <= tip),
                "{label}: {} reopened at {index_tip:?}",
                id.name()
            );
        }
        assert_eq!(
            store
                .subscribe_watermark()
                .borrow()
                .tip
                .map(|tip| tip.height),
            Some(before_cut),
            "{label}: the watermark is the lowest tip"
        );
        assert_each_at_its_tip(&chain, &storage, &tips, label).await;

        store.build_to(tip).await.expect("resumes");
        assert_eq!(stored(&storage.inner, &every_index()), expected, "{label}");
    }
}

#[tokio::test]
async fn an_index_whose_commit_failed_builds_again_in_the_same_process() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let ids = every_index();
    let storage = Faulty::default();
    let store = boot(&storage, &chain).await;
    storage.fail_commit_after(IndexId::Spent, cut(tip));
    store.build_to(tip).await.expect_err("the commit failed");
    store.build_to(tip).await.expect("the next build finishes");
    assert_eq!(
        stored(&storage.inner, &ids),
        oracle(&chain, &ids, tip).await
    );
}
