//! Era composition of the compact blocks a real tonic client receives from a
//! running zainod: the served range is complete and contiguous, and each height's
//! coinbase pays its era's pool (orchard-only, ironwood-only, and the
//! orchard→ironwood flip at the NU6.3 activation height).
//!
//! Known failure: served ironwood actions are missing at the first ironwood-era
//! height — <https://github.com/zingolabs/zaino/issues/1368>.

use std::time::Duration;

use anyhow::Result;
use zaino_testutils::{
    ActivationHeights, Backend, ChainCache, Fixture, Pool, Validator as _, Zainod, Zebrad,
    REGTEST_ACTIVATION_HEIGHTS,
};

const READY: Duration = Duration::from_secs(300);

/// The mid-chain NU6.3 (Ironwood) activation height for the transition fixture:
/// an Orchard era `[2, 6)` that flips to Ironwood at height 6.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

fn nu6_3_boundary() -> Fixture {
    Fixture::boundary(
        "nu6_3",
        ActivationHeights {
            nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
            nu7: None,
            ..REGTEST_ACTIVATION_HEIGHTS
        },
        NU6_3_TRANSITION_BOUNDARY,
    )
}

/// Below the NU6.3 boundary the orchard-receiver coinbase stays in Orchard
/// actions for every block.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_wire_serving_zebrad() -> Result<()> {
    let cache = ChainCache::ensure_fixture(Fixture::orchard_only()).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    let tip = cache.manifest.fixture.height;
    assert_eq!(
        cache.manifest.fixture.heights.nu6_3, None,
        "the orchard-only fixture never activates NU6.3"
    );
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        assert_eq!(
            orchard > 0,
            height >= 2,
            "orchard actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
        assert_eq!(
            ironwood, 0,
            "no ironwood actions anywhere at height {height} (orchard {orchard})"
        );
    }
    Ok(())
}

/// The main fixture activates NU6.3 at height 2, so the orchard-receiver
/// coinbase is paid as Ironwood actions from the first shielded block and no
/// block in the chain carries an Orchard action.
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_wire_serving_zebrad() -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    let ironwood_heights: Vec<u64> = cache
        .manifest
        .events
        .iter()
        .filter(|event| event.pool == Pool::Ironwood)
        .map(|event| u64::from(event.height))
        .collect();
    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        if height <= 2 || ironwood_heights.contains(&height) {
            assert_eq!(
                ironwood > 0,
                height >= 2,
                "ironwood actions at height {height} (orchard {orchard}, ironwood {ironwood})"
            );
        }
        assert_eq!(
            orchard, 0,
            "no orchard actions anywhere at height {height} (ironwood {ironwood})"
        );
    }
    Ok(())
}

/// The unchanged orchard-receiver miner's served stream flips from Orchard to
/// Ironwood actions exactly at the NU6.3 activation height.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_to_ironwood_transition_wire_serving_zebrad() -> Result<()> {
    let cache = ChainCache::ensure_fixture(nu6_3_boundary()).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    assert_eq!(
        zebrad.coinbase_pool(),
        cache.manifest.fixture.coinbase,
        "the fixture's miner must stay the orchard receiver across the boundary"
    );

    // Two blocks past the boundary, so both eras carry more than one block.
    let target = NU6_3_TRANSITION_BOUNDARY + 2;
    let tip = zebrad
        .generate_blocks(target - cache.manifest.fixture.height)
        .await?;
    assert_eq!(tip, target, "mined to {tip}, wanted {target}");
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let boundary = u64::from(NU6_3_TRANSITION_BOUNDARY);
        assert_eq!(
            orchard > 0,
            (2..boundary).contains(&height),
            "orchard actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
        assert_eq!(
            ironwood > 0,
            height >= boundary,
            "ironwood actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
    }
    Ok(())
}
