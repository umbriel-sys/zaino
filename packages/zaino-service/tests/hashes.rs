//! getblockhash and getblockhashes from Zaino's headers; getnetworkinfo and
//! ping from the validator.

mod common;

use common::{chain, local, TIP};
use zaino_primitives::types::{BlockHash, Height};
use zaino_service::{BlockHashAt, ServiceError};
use zaino_source::OneShotGetNetworkInfo as _;
use zaino_source_mock::MockChain;

fn at(chain: &MockChain, h: u32) -> (BlockHash, u32) {
    let block = chain.block_at(h).expect("on the chain");
    (block.hash(), block.block.header.time)
}

/// getblockhash: a height from genesis, or from the tip when negative;
/// outside the chain refused.
#[tokio::test]
async fn block_hashes_by_index_count_from_either_end() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let tip = i32::try_from(TIP).expect("small");
    for (index, height) in [(0, 0), (150, 150), (tip, TIP), (-1, TIP), (-tip - 1, 0)] {
        assert_eq!(
            snapshot.block_hash(index).await.expect("in range"),
            at(&chain, height).0,
            "{index}"
        );
    }
    for index in [tip + 1, -tip - 2, i32::MIN] {
        assert!(
            matches!(
                snapshot.block_hash(index).await,
                Err(ServiceError::InvalidParameter(_))
            ),
            "{index}"
        );
    }
}

/// getblockhashes: exactly the blocks whose time lies in the range, by time
/// then hash; nothing for an empty, inverted or out-of-chain range.
#[tokio::test]
async fn block_hashes_by_time_are_every_block_in_range() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let every: Vec<BlockHashAt> = (0..=TIP)
        .map(|h| {
            let (hash, time) = at(&chain, h);
            BlockHashAt {
                height: Height::try_from(h).expect("small"),
                hash,
                time,
            }
        })
        .collect();
    let expected = |low: u32, high: u32| {
        let mut hits: Vec<BlockHashAt> = every
            .iter()
            .copied()
            .filter(|block| low <= block.time && block.time < high)
            .collect();
        hits.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.hash.cmp(&b.hash)));
        hits
    };
    let time = |h: u32| at(&chain, h).1;
    for (low, high) in [
        (time(0), time(TIP) + 1),
        (time(100), time(110)),
        (time(250), time(251)),
        (time(TIP), u32::MAX),
        (0, time(0)),
        (time(10), time(10)),
        (time(20), time(10)),
        (time(TIP) + 1, u32::MAX),
    ] {
        assert_eq!(
            snapshot.block_hashes(low, high).await.expect("answers"),
            expected(low, high),
            "[{low}, {high})"
        );
    }
    assert_eq!(
        snapshot
            .block_hashes(time(0), time(TIP) + 1)
            .await
            .expect("answers")
            .len(),
        usize::try_from(TIP + 1).expect("small")
    );
}

/// getnetworkinfo and ping are the validator's.
#[tokio::test]
async fn network_info_and_ping_pass_through() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let pinged = chain.calls("ping");
    snapshot.ping().await.expect("answers");
    assert_eq!(chain.calls("ping"), pinged + 1);
    assert_eq!(
        snapshot.network_info().await.expect("answers"),
        chain.get_network_info().await.expect("the validator's")
    );
}
