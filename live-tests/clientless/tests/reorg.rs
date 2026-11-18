//! Reorgs the validator performs while zaino is serving: the served chain
//! follows, the mempool stays coherent, and the finalised store never
//! rewinds. Zebra reorgs at most 1,000 deep; the fixture's `head-b` is one
//! block longer than `head-a`, so feeding it to a `head-a` node is the
//! deepest reorg zebra will ever do, two blocks above zaino's seam.

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use zaino_testutils::{Backend, ChainCache, Validator as _, Zainod, Zebrad, FILLER_ADDRESS};

const READY: Duration = Duration::from_secs(120);

/// The served tip equals the validator's, by height and hash.
async fn assert_follows(indexer: &Zainod, zebrad: &Zebrad, label: &str) -> Result<()> {
    let height = zebrad.chain_height().await?;
    indexer.wait_for_block(height, READY).await?;
    let served = indexer
        .json_rpc()
        .call_value("getbestblockhash", json!([]))
        .await?;
    assert_eq!(
        served.as_str().context("getbestblockhash is a string")?,
        zebrad.best_block_hash().await?,
        "{label}: served tip must be the validator's"
    );
    let range = indexer.get_block_range(height - 5, height).await?;
    for pair in range.windows(2) {
        assert_eq!(
            pair[1].prev_hash, pair[0].hash,
            "{label}: served chain links"
        );
    }
    Ok(())
}

#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn a_one_block_reorg_is_followed(#[case] backend: Backend) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let base = cache.manifest.fixture.height;

    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;
    let own = zebrad.best_block_hash().await?;

    let branch = cache.head_b_blocks()?;
    zebrad.submit_block(&branch[0]).await?;
    let reorged = zebrad.submit_block(&branch[1]).await?;
    assert_eq!(reorged, base + 2);
    assert_ne!(
        zebrad.best_block_hash().await?,
        own,
        "the validator switched branch"
    );

    assert_follows(&indexer, &zebrad, "1-deep").await
}

#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn a_ten_block_reorg_is_followed(#[case] backend: Backend) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let base = cache.manifest.fixture.height;

    let tip = zebrad.generate_blocks_to(10, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;
    let own = zebrad.best_block_hash().await?;

    let branch = cache.head_b_blocks()?;
    let mut height = base;
    for block in &branch[..11] {
        height = zebrad.submit_block(block).await?;
    }
    assert_eq!(height, base + 11);
    assert_ne!(
        zebrad.best_block_hash().await?,
        own,
        "the validator switched branch"
    );

    assert_follows(&indexer, &zebrad, "10-deep").await
}

/// Zebra's deepest reorg, two blocks above zaino's seam: the served chain
/// follows and the finalised store only ever grows, keeping every block it
/// holds.
#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn the_deepest_reorg_leaves_the_finalised_store_alone(
    #[case] backend: Backend,
) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let head_a = cache
        .manifest
        .head_a
        .clone()
        .context("head-a in the fixture")?;
    let zebrad = Zebrad::launch(cache.head_a_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    indexer.wait_for_block(head_a.height, READY).await?;
    assert_eq!(zebrad.best_block_hash().await?, head_a.tip_hash);
    let finalised_before = indexer
        .finalised_tip_height()
        .context("the store holds a finalised band at launch")?;
    assert!(
        finalised_before < cache.manifest.fixture.height,
        "the finalised tip must sit below the fork point"
    );
    let held_before = indexer.get_block(finalised_before).await?.hash;

    let mut height = cache.manifest.fixture.height;
    for block in cache.head_b_blocks()? {
        height = zebrad.submit_block(&block).await?;
    }
    let head_b = cache
        .manifest
        .head_b
        .clone()
        .context("head-b in the fixture")?;
    assert_eq!(height, head_b.height);
    assert_eq!(
        zebrad.best_block_hash().await?,
        head_b.tip_hash,
        "the validator reorged to head-b"
    );

    assert_follows(&indexer, &zebrad, "999-deep").await?;
    let finalised_after = indexer
        .finalised_tip_height()
        .context("the store still holds a finalised band")?;
    assert!(
        finalised_after >= finalised_before,
        "a reorg above the seam must not rewind the finalised store \
         ({finalised_before} -> {finalised_after})"
    );
    assert_eq!(
        indexer.get_block(finalised_before).await?.hash,
        held_before,
        "the finalised block at {finalised_before} must be the one held before the reorg"
    );
    Ok(())
}

/// The validator is replaced under a running indexer by one on the other
/// branch at the same height; the indexer serves the new chain.
#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn a_respawned_validator_on_another_branch_is_followed(
    #[case] backend: Backend,
) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let base = cache.manifest.fixture.height;

    let tip = zebrad.generate_blocks_to(3, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;

    let head_a = cache
        .manifest
        .head_a
        .clone()
        .context("head-a in the fixture")?;
    let zebrad = zebrad
        .respawn_from(&cache.head_a_state_dir(), head_a.height)
        .await?;
    assert_eq!(zebrad.chain_height().await?, head_a.height);
    assert!(head_a.height > base + 3);

    assert_follows(&indexer, &zebrad, "respawn").await
}
