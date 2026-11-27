use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::{
    Backend, ChainCache, ShieldedProtocol, Validator as _, Zainod, Zebrad, FILLER_ADDRESS,
};

const READY: Duration = Duration::from_secs(90);

/// Builds the shared fixture, so a cold run pays for it once here, before the
/// suite. On a warm cache it only verifies the restore matches the manifest.
#[tokio::test(flavor = "multi_thread")]
async fn build_chain_cache() -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    assert_eq!(zebrad.chain_height().await?, cache.manifest.fixture.height);
    assert_eq!(zebrad.best_block_hash().await?, cache.manifest.tip_hash);
    assert_eq!(
        zebrad.activation_heights().await?,
        cache.manifest.fixture.heights,
        "the restored node must report the cached schedule"
    );
    let head_a = cache.manifest.head_a.as_ref().context("head-a built")?;
    let head_b = cache.manifest.head_b.as_ref().context("head-b built")?;
    assert_eq!(head_b.height, head_a.height + 1);
    assert_eq!(
        cache.head_b_blocks()?.len() as u32,
        head_b.height - cache.manifest.fixture.height
    );
    Ok(())
}

mod chain_query_interface {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let range = indexer.get_block_range(1, tip).await?;
        assert_eq!(
            range.len(),
            tip as usize,
            "get_block_range must serve every block over [1, tip]"
        );
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(
                block.height,
                (offset + 1) as u64,
                "blocks must be contiguous from height 1"
            );
            assert_eq!(block.hash.len(), 32, "block hash must be 32 bytes");
        }
        Ok(())
    }

    /// Ephemeral mode opens no persistent finalised-state database: every read that
    /// would need one is passed through to the validator instead.
    ///
    /// `db_height` is pinned at `0` in ephemeral mode, so the non-finalised cache
    /// retains only the window above the seam. The fixture's chain is past that,
    /// so the reads below the floor are genuinely served by the passthrough and
    /// not by the cache.
    #[tokio::test(flavor = "multi_thread")]
    async fn ephemeral_serves_finalised_blocks_zebrad() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer =
            Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch).ephemeral()).await?;
        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;
        let cache_floor = tip - zaino_consensus_seam();

        assert_eq!(
            indexer.finalised_tip_height(),
            None,
            "ephemeral mode must open no finalised database, but the finalised writer \
             reported a committed tip"
        );

        let range = indexer.get_block_range(1, tip).await?;
        assert_eq!(
            range.len(),
            tip as usize,
            "ephemeral index must serve every block over [1, {tip}]"
        );
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(
                block.height,
                (offset + 1) as u64,
                "served blocks must be contiguous from height 1"
            );
        }
        for pair in range.windows(2) {
            assert_eq!(
                pair[1].prev_hash, pair[0].hash,
                "block {} does not link to block {} — the passthrough and the cache \
                 served two different chains",
                pair[1].height, pair[0].height
            );
        }

        let below_floor = cache_floor / 2;
        let by_height = indexer.get_block(below_floor).await?;
        let hash_bytes: [u8; 32] = by_height
            .hash
            .clone()
            .try_into()
            .ok()
            .context("served block hash must be 32 bytes")?;
        let by_hash = indexer.get_block_by_hash(hash_bytes).await?;
        assert_eq!(
            by_hash, by_height,
            "a block below the cache floor must be the same fetched by height and by hash"
        );

        let hash = zebrad
            .rpc()
            .call_value("getblockhash", json!([by_height.height]))
            .await?;
        let mut served = by_height.hash.clone();
        served.reverse();
        assert_eq!(
            zaino_testutils::hex::encode(&served),
            hash.as_str().context("getblockhash returns a hex string")?,
            "the passthrough served a different block than the validator holds at {}",
            by_height.height
        );
        Ok(())
    }

    fn zaino_consensus_seam() -> u32 {
        1_001
    }

    fn validator_subtrees(reply: &Value) -> Result<Vec<(Vec<u8>, u64)>> {
        reply
            .get("subtrees")
            .and_then(Value::as_array)
            .context("z_getsubtreesbyindex reply missing `subtrees` array")?
            .iter()
            .map(|subtree| {
                let root_hex = subtree
                    .get("root")
                    .and_then(Value::as_str)
                    .context("subtree root from validator is not a string")?;
                let bytes = zaino_testutils::hex::decode(root_hex, "z_getsubtreesbyindex root")?;
                let end_height = subtree
                    .get("end_height")
                    .and_then(Value::as_u64)
                    .context("subtree missing end_height")?;
                Ok((bytes, end_height))
            })
            .collect()
    }

    fn indexer_subtrees(roots: &[zaino_proto::proto::service::SubtreeRoot]) -> Vec<(Vec<u8>, u64)> {
        roots
            .iter()
            .map(|r| (r.root_hash.clone(), r.completing_block_height))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        indexer
            .wait_for_block(cache.manifest.fixture.height, READY)
            .await?;

        let vrpc = zebrad.rpc();

        let test_pools = [
            ("sapling", ShieldedProtocol::Sapling),
            ("orchard", ShieldedProtocol::Orchard),
        ];
        let valid_start_index: u32 = 0;
        let max_entries: u32 = 0;

        // *** Test valid requests ***
        for (pool_string, protocol) in test_pools {
            let indexer_roots = indexer
                .get_subtree_roots(valid_start_index, protocol, max_entries)
                .await?;
            let validator_reply = vrpc
                .call_value(
                    "z_getsubtreesbyindex",
                    json!([pool_string, valid_start_index, max_entries]),
                )
                .await?;
            assert_eq!(
                indexer_subtrees(&indexer_roots),
                validator_subtrees(&validator_reply)?,
                "chain-index subtree roots must match validator for pool {pool_string}"
            );
        }

        // *** Test invalid requests ***
        let invalid_start_index: u32 = 10000;
        let (orchard_string, orchard_protocol) = test_pools[1];
        let indexer_roots = indexer
            .get_subtree_roots(invalid_start_index, orchard_protocol, max_entries)
            .await?;
        let validator_reply = vrpc
            .call_value(
                "z_getsubtreesbyindex",
                json!([orchard_string, invalid_start_index, max_entries]),
            )
            .await?;
        assert_eq!(
            indexer_subtrees(&indexer_roots),
            validator_subtrees(&validator_reply)?,
            "chain-index subtree roots must match validator at invalid start index"
        );
        Ok(())
    }

    /// The mempool stream closes on every tip change; one block per round.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        indexer
            .wait_for_block(cache.manifest.fixture.height, READY)
            .await?;

        for iteration in 0..5 {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let stream = indexer.get_mempool_stream();
            let mine = async {
                let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
                indexer.wait_for_block(tip, READY).await
            };

            let joined = tokio::time::timeout(Duration::from_secs(20), async {
                tokio::join!(stream, mine)
            })
            .await;
            let (stream_result, mine_result) = joined.unwrap_or_else(|_| {
                panic!(
                    "mempool stream did not close after chain tip changed on iteration {iteration}"
                )
            });
            stream_result.with_context(|| {
                format!("mempool stream yielded unexpected error on iteration {iteration}")
            })?;
            mine_result?;
        }
        Ok(())
    }

    /// The indexer serves exactly each new block as the tip advances.
    #[tokio::test(flavor = "multi_thread")]
    async fn zallet_like_steady_state_loop() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let mut prev_tip = cache.manifest.fixture.height;
        indexer.wait_for_block(prev_tip, READY).await?;

        for iteration in 0..5 {
            let current_tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
            indexer.wait_for_block(current_tip, READY).await?;

            assert_eq!(
                indexer.latest_block_height().await?,
                current_tip,
                "indexer must track the advancing tip on iteration {iteration}"
            );

            let applied = indexer.get_block_range(prev_tip + 1, current_tip).await?;
            assert_eq!(
                applied.len(),
                (current_tip - prev_tip) as usize,
                "indexer must serve exactly the newly mined blocks on iteration {iteration}"
            );
            for (offset, block) in applied.iter().enumerate() {
                assert_eq!(
                    block.height,
                    u64::from(prev_tip + 1 + offset as u32),
                    "applied blocks must be contiguous on iteration {iteration}"
                );
            }

            prev_tip = current_tip;
        }
        Ok(())
    }
}
