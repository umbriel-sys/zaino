//! Mempool reads: listings from the tip-agnostic set, transactions and their
//! status placed against the snapshot through the coherent view, and the
//! coherent stream.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::{all_local, chain, config, disk, eventually, validator_backed, TIP};
use futures::StreamExt as _;
use zaino_chain_store_service::NamedSet;
use zaino_persistence_lsm_tree::fs::SimFs;
use zaino_primitives::types::rpc::SpentOutpoint;
use zaino_primitives::types::{Height, TransactionId};
use zaino_service::{Mode, Placement, Service, ServiceError, Spend};
use zaino_source::OneShotGetSpentInfo as _;
use zaino_source_mock::MockChain;

/// The validator serves through `SERVED`; its mempool is the next block's
/// spends.
const SERVED: u32 = TIP - 10;

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small")
}

/// The next block's spends: the mock's mempool at `SERVED`, with bytes.
fn pending(chain: &MockChain) -> Vec<(TransactionId, Vec<u8>)> {
    let block = chain.block_at(SERVED + 1).expect("held");
    let raw = block.raw.as_ref().expect("built blocks carry bytes");
    block
        .block
        .transactions()
        .iter()
        .zip(&raw.transactions)
        .skip(1)
        .map(|(tx, bytes)| (tx.txid, bytes.clone()))
        .collect()
}

/// A service with no store, its head and coherent mempool at `SERVED`.
async fn served(chain: &MockChain) -> Service<MockChain> {
    chain.serve_to(SERVED);
    let service = Service::spawn(Arc::new(chain.clone()), config(validator_backed()))
        .await
        .expect("spawns");
    let expected: HashSet<_> = pending(chain).into_iter().map(|(txid, _)| txid).collect();
    assert!(
        eventually(|| {
            let snapshot = service.snapshot();
            u32::from(snapshot.tip().height) == SERVED
                && snapshot
                    .mempool_txids()
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>()
                    == expected
                && snapshot.mempool_stream(true).is_some()
        })
        .await,
        "the mempool coheres at {SERVED}"
    );
    service
}

/// Listings: every pending txid; entries but the excluded; a malformed
/// exclude refused.
#[tokio::test]
async fn listings_come_from_the_mempool() {
    let chain = chain();
    let service = served(&chain).await;
    let snapshot = service.snapshot();
    let pending = pending(&chain);
    assert!(pending.len() >= 2, "a spend block");
    let excluded = pending[0].0;
    let entries: HashSet<TransactionId> = snapshot
        .mempool_entries(&[<[u8; 32]>::from(excluded).to_vec()])
        .expect("a valid suffix")
        .iter()
        .map(|entry| entry.txid)
        .collect();
    let expected: HashSet<_> = pending.iter().skip(1).map(|(txid, _)| *txid).collect();
    assert_eq!(entries, expected);
    assert!(matches!(
        snapshot.mempool_entries(&[vec![0; 33]]),
        Err(ServiceError::InvalidParameter(_))
    ));
    assert!(snapshot.mempool_info().size >= pending.len() as u64);
}

/// A pending transaction comes from the coherent mempool, to be mined at the
/// tip's successor, and is placed there; a mined one from the chain.
#[tokio::test]
async fn transactions_are_placed_against_the_snapshot() {
    let chain = chain();
    let service = served(&chain).await;
    let snapshot = service.snapshot();
    for (txid, bytes) in pending(&chain) {
        let transaction = snapshot
            .transaction(txid)
            .await
            .expect("answers")
            .expect("pending");
        assert!(transaction.mempool);
        assert_eq!(transaction.bytes, bytes);
        assert_eq!(transaction.height, height(SERVED + 1));
        let status = snapshot.transaction_status(txid).await.expect("answers");
        assert_eq!(
            status.best,
            Some(Placement::Mempool(Some(height(SERVED + 1))))
        );
        assert!(status.elsewhere.is_empty());
    }
    let mined = chain.block_at(SERVED).expect("held").block.transactions()[1].txid;
    let transaction = snapshot
        .transaction(mined)
        .await
        .expect("answers")
        .expect("mined");
    assert!(!transaction.mempool);
    assert_eq!(transaction.height, height(SERVED));
    assert!(matches!(
        snapshot.transaction_status(mined).await.expect("answers").best,
        Some(Placement::Block(position)) if position.block.height == height(SERVED)
    ));
    assert_eq!(
        snapshot
            .transaction(TransactionId::from([9; 32]))
            .await
            .expect("answers"),
        None
    );
}

/// Against a snapshot older than the mempool's tip, a pending transaction is
/// placed off that snapshot's chain, and reading it asks for a fresh
/// snapshot rather than answer from a mempool for another chain state.
#[tokio::test]
async fn an_older_snapshot_places_the_mempool_elsewhere() {
    let chain = chain();
    let service = served(&chain).await;
    let old = service.snapshot();
    chain.serve_to(SERVED + 1);
    let next = chain
        .block_at(SERVED + 2)
        .expect("held")
        .block
        .transactions()[1]
        .txid;
    assert!(
        eventually(|| {
            let snapshot = service.snapshot();
            u32::from(snapshot.tip().height) == SERVED + 1
                && snapshot.mempool_txids().contains(&next)
                && snapshot.mempool_stream(true).is_some()
        })
        .await,
        "the mempool coheres at the new tip"
    );
    let status = old.transaction_status(next).await.expect("answers");
    assert_eq!(status.best, None);
    assert_eq!(
        status.elsewhere,
        vec![Placement::Mempool(Some(height(SERVED + 2)))],
        "at the successor of the mempool's tip, which the chain holds now"
    );
    assert!(matches!(
        old.transaction(next).await,
        Err(ServiceError::MempoolIncoherent)
    ));
    let fresh = service.snapshot();
    assert_eq!(
        fresh.transaction_status(next).await.expect("answers").best,
        Some(Placement::Mempool(Some(height(SERVED + 2))))
    );
}

/// The first transparent output the block at `at` spends after its
/// coinbase, and the transaction spending it with its first input.
fn first_spend(chain: &MockChain, at: u32) -> (SpentOutpoint, TransactionId) {
    let block = chain.block_at(at).expect("held");
    let spender = &block.block.transactions()[1];
    let input = &spender.transparent.inputs[0];
    let spent = SpentOutpoint {
        txid: input.prev_txid,
        index: input.prev_index,
    };
    (spent, spender.txid)
}

/// getspentinfo with no store (as while one builds): a spend in the coherent
/// mempool is answered from it, the validator not asked; once mined, the
/// validator's; a snapshot the mempool is not coherent with is refused.
#[tokio::test]
async fn a_mempool_spend_is_answered_from_the_mempool() {
    let chain = chain();
    let service = served(&chain).await;
    let old = service.snapshot();
    let (spent, spender) = first_spend(&chain, SERVED + 1);
    let asked = chain.calls("get_spent_info");
    assert_eq!(
        old.spent_info(spent).await.expect("answers"),
        Some(Spend::Mempool {
            txid: spender,
            index: 0
        })
    );
    assert_eq!(chain.calls("get_spent_info"), asked, "the mempool answers");

    chain.serve_to(SERVED + 1);
    let (next_spent, next_spender) = first_spend(&chain, SERVED + 2);
    assert!(
        eventually(|| {
            let snapshot = service.snapshot();
            u32::from(snapshot.tip().height) == SERVED + 1
                && snapshot.mempool_txids().contains(&next_spender)
                && snapshot.mempool_stream(true).is_some()
        })
        .await,
        "the mempool coheres at the new tip"
    );
    let fresh = service.snapshot();
    let mined = chain.get_spent_info(spent).await.expect("the validator's");
    assert_eq!(mined.height, height(SERVED + 1));
    assert_eq!(
        fresh.spent_info(spent).await.expect("answers"),
        Some(Spend::Mined(mined))
    );
    assert_eq!(
        fresh.spent_info(next_spent).await.expect("answers"),
        Some(Spend::Mempool {
            txid: next_spender,
            index: 0
        })
    );
    assert!(matches!(
        old.spent_info(next_spent).await,
        Err(ServiceError::MempoolIncoherent)
    ));
}

/// getspentinfo over a built store: the spend index says unspent, the
/// coherent mempool names the spender, the validator is not asked; a mined
/// spend is still the index's.
#[tokio::test]
async fn a_mempool_spend_is_found_past_the_spend_index() {
    let chain = chain();
    chain.serve_to(SERVED);
    let service = Service::spawn(
        Arc::new(chain.clone()),
        config(disk(&SimFs::new(), NamedSet::Full, Mode::Primary)),
    )
    .await
    .expect("spawns");
    let (spent, spender) = first_spend(&chain, SERVED + 1);
    assert!(
        eventually(|| {
            let snapshot = service.snapshot();
            all_local(&service)
                && snapshot.mempool_txids().contains(&spender)
                && snapshot.mempool_stream(true).is_some()
        })
        .await,
        "built, and the mempool coheres"
    );
    let snapshot = service.snapshot();
    let (mined_spent, _) = first_spend(&chain, SERVED);
    let mined = chain
        .get_spent_info(mined_spent)
        .await
        .expect("the validator's");
    let asked = chain.calls("get_spent_info");
    assert_eq!(
        snapshot.spent_info(spent).await.expect("answers"),
        Some(Spend::Mempool {
            txid: spender,
            index: 0
        })
    );
    assert_eq!(
        snapshot.spent_info(mined_spent).await.expect("answers"),
        Some(Spend::Mined(mined))
    );
    assert_eq!(chain.calls("get_spent_info"), asked, "local");
}

/// The pinned stream yields the coherent mempool and ends when the tip
/// moves; a snapshot the mempool is not coherent with gets none.
#[tokio::test]
async fn the_stream_ends_when_the_tip_moves() {
    let chain = chain();
    let service = served(&chain).await;
    let snapshot = service.snapshot();
    let stream = snapshot.mempool_stream(true).expect("coherent");
    chain.serve_to(SERVED + 1);
    let streamed: Vec<_> = stream.collect().await;
    let bytes: HashSet<Vec<u8>> = streamed
        .into_iter()
        .map(|item| item.expect("no lag").to_vec())
        .collect();
    let expected: HashSet<Vec<u8>> = pending(&chain)
        .into_iter()
        .map(|(_, bytes)| bytes)
        .collect();
    assert!(expected.is_subset(&bytes), "every pending transaction");
    assert!(
        eventually(|| u32::from(service.snapshot().tip().height) == SERVED + 1).await,
        "the head moves"
    );
    assert!(
        snapshot.mempool_stream(true).is_none(),
        "the old snapshot is not coherent with the moved mempool"
    );
}
