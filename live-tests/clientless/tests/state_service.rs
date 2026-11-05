//! Fetch / State backend parity over the main fixture: one zebrad, both
//! indexers on the cached database, every read at heights the manifest names.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::{
    assert_json_shape_matches, assert_rpc_parity, Backend, ChainCache, Pool, ShieldedProtocol,
    Zainod, Zebrad,
};

/// Both indexers resync the non-finalised band above the cached database.
const READY: Duration = Duration::from_secs(300);
/// zainod's non-finalised depth: heights at or below `tip - SEAM_DEPTH` are
/// served from the finalised store.
const SEAM_DEPTH: u32 = 1_001;

// The testnet parity suite lives in its own target, `tests/testnet_parity.rs`:
// it shares no topology with the regtest tests here, and consolidating its
// eleven tests into one env per fixture left it self-contained.

/// The heights a parity read visits: genesis, the first blocks, every recorded
/// transaction, both sides of the seam and the tip.
fn parity_heights(cache: &ChainCache) -> Vec<u32> {
    let tip = cache.manifest.fixture.height;
    let seam = tip - SEAM_DEPTH;
    let mut heights = vec![0, 1, 2, seam - 1, seam, seam + 1, tip - 1, tip];
    heights.extend(cache.manifest.events.iter().map(|event| event.height));
    heights.sort_unstable();
    heights.dedup();
    heights
}

/// The height band around the seam holding the mid-chain activity burst.
fn seam_band(cache: &ChainCache) -> (u32, u32) {
    let seam = cache.manifest.fixture.height - SEAM_DEPTH;
    let start = cache
        .manifest
        .events
        .iter()
        .map(|event| event.height)
        .filter(|&height| height < seam)
        .max()
        .map_or(seam - 1, |height| height.min(seam - 1));
    (start, seam + 1)
}

mod zebra {

    use super::*;

    pub(crate) mod check_info {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn regtest_no_cache() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;

            let frpc = fetch.json_rpc();
            let srpc = state.json_rpc();
            // `errors` carries zebra's latest transient sync warning, which rotates on a
            // peerless regtest node, so the two sides sample different text.
            assert_rpc_parity("getinfo", "", &frpc, &srpc, &["errorstimestamp", "errors"]).await?;

            // TODO: Fix this! (ignored due to [https://github.com/zingolabs/zaino/issues/235]).
            // assert_eq!(
            //     fetch_service_blockchain_info.value_pools(),
            //     state_service_blockchain_info.value_pools()
            // );
            let fetch_info = frpc.call_value("getblockchaininfo", json!([])).await?;
            let state_info = srpc.call_value("getblockchaininfo", json!([])).await?;
            assert_eq!(
                fetch_info.get("blocks").and_then(Value::as_u64),
                Some(u64::from(tip)),
                "fetch getblockchaininfo.blocks must be the fixture tip"
            );
            assert_eq!(
                fetch_info.get("bestblockhash").and_then(Value::as_str),
                Some(cache.manifest.tip_hash.as_str()),
                "fetch getblockchaininfo.bestblockhash must be the fixture tip hash"
            );
            assert_json_shape_matches(
                "getblockchaininfo",
                fetch_info,
                state_info,
                &[],
                &[
                    "chain",
                    "blocks",
                    "bestblockhash",
                    "estimatedheight",
                    "upgrades",
                    "consensus",
                ],
            );
            Ok(())
        }
    }

    pub(crate) mod get {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn best_blockhash() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let hash = assert_rpc_parity(
                "getbestblockhash",
                "",
                &fetch.json_rpc(),
                &state.json_rpc(),
                &[],
            )
            .await?;
            assert_eq!(
                hash.as_str(),
                Some(cache.manifest.tip_hash.as_str()),
                "getbestblockhash must be the fixture tip hash"
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_count() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let count = assert_rpc_parity(
                "getblockcount",
                "",
                &fetch.json_rpc(),
                &state.json_rpc(),
                &[],
            )
            .await?;
            assert_eq!(
                count.as_u64(),
                Some(u64::from(tip)),
                "getblockcount must be the fixture tip"
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn mining_info() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let frpc = fetch.json_rpc();
            let srpc = state.json_rpc();
            let ignore = &["errorstimestamp"];
            let info = assert_rpc_parity("getmininginfo", "", &frpc, &srpc, ignore).await?;
            assert_eq!(
                info.get("blocks").and_then(Value::as_u64),
                Some(u64::from(tip)),
                "getmininginfo.blocks must be the fixture tip"
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn difficulty() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            assert_rpc_parity(
                "getdifficulty",
                "",
                &fetch.json_rpc(),
                &state.json_rpc(),
                &[],
            )
            .await?;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_network_sol_ps() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            assert_rpc_parity(
                "getnetworksolps",
                "",
                &fetch.json_rpc(),
                &state.json_rpc(),
                &[],
            )
            .await?;
            Ok(())
        }

        /// A proper test would boot up multiple nodes at the same time, and ask each node
        /// for information about its peers. In the current state, this test does nothing.
        #[tokio::test(flavor = "multi_thread")]
        async fn peer_info() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            assert_rpc_parity("getpeerinfo", "", &fetch.json_rpc(), &state.json_rpc(), &[]).await?;
            Ok(())
        }

        mod z {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            pub(crate) async fn z_validate_address() -> Result<()> {
                let cache = ChainCache::ensure().await?;
                let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
                let _fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
                let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

                clientless::rpc::z_validate_address::run_z_validate_for(&state.json_rpc()).await
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_object_regtest() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let frpc = fetch.json_rpc();
            let srpc = state.json_rpc();
            for height in parity_heights(&cache) {
                let params = format!(r#"["{height}", 1]"#);
                let block = assert_rpc_parity("getblock", &params, &frpc, &srpc, &[]).await?;
                let hash = block
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({height},1).hash"))?;
                let by_hash = srpc.call_value("getblock", json!([hash, 1])).await?;
                assert_eq!(
                    by_hash, block,
                    "state getblock by-hash must equal by-height at {height}"
                );
                let txids: Vec<&str> = block
                    .get("tx")
                    .and_then(Value::as_array)
                    .with_context(|| format!("getblock({height},1).tx"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                for event in cache.manifest.events_at(height) {
                    assert!(
                        txids.contains(&event.txid.as_str()),
                        "block {height} must carry the recorded {} {}: {txids:?}",
                        event.kind,
                        event.txid
                    );
                }
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_raw_regtest() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let frpc = fetch.json_rpc();
            let srpc = state.json_rpc();
            for height in parity_heights(&cache) {
                let params = format!(r#"["{height}", 0]"#);
                assert_rpc_parity("getblock", &params, &frpc, &srpc, &[]).await?;
            }
            Ok(())
        }
    }

    pub(crate) mod lightwallet_indexer {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_block() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            assert_eq!(
                fetch.latest_block_height().await?,
                state.latest_block_height().await?,
                "latest block height must agree"
            );
            assert_eq!(
                fetch.latest_block_height().await?,
                tip,
                "latest block height must be the fixture tip"
            );
            assert_eq!(
                fetch.get_block(tip).await?,
                state.get_block(tip).await?,
                "tip block must agree"
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;

            for height in parity_heights(&cache) {
                let by_height_f = fetch.get_block(height).await?;
                let by_height_s = state.get_block(height).await?;
                assert_eq!(by_height_f, by_height_s, "get_block({height}) must agree");

                let hash_bytes: [u8; 32] = by_height_f
                    .hash
                    .clone()
                    .try_into()
                    .ok()
                    .with_context(|| format!("block {height} hash must be 32 bytes"))?;
                let by_hash_f = fetch.get_block_by_hash(hash_bytes).await?;
                let by_hash_s = state.get_block_by_hash(hash_bytes).await?;
                assert_eq!(
                    by_hash_f, by_hash_s,
                    "get_block by-hash must agree at {height}"
                );
                assert_eq!(
                    by_hash_f, by_height_f,
                    "by-hash must equal by-height at {height}"
                );
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let frpc = fetch.json_rpc();
            let srpc = state.json_rpc();
            for height in parity_heights(&cache) {
                let blk = frpc
                    .call_value("getblock", json!([height.to_string(), 1]))
                    .await?;
                let hash = blk
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({height},1).hash missing"))?
                    .to_string();
                let params = format!(r#"["{hash}", false]"#);
                assert_rpc_parity("getblockheader", &params, &frpc, &srpc, &[]).await?;
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_tree_state() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            for height in parity_heights(&cache) {
                if height == 0 {
                    continue;
                }
                assert_eq!(
                    fetch.get_tree_state(height).await?,
                    state.get_tree_state(height).await?,
                    "tree state at {height} must agree"
                );
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_subtree_roots() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            for protocol in [ShieldedProtocol::Sapling, ShieldedProtocol::Orchard] {
                assert_eq!(
                    fetch.get_subtree_roots(0, protocol, 0).await?,
                    state.get_subtree_roots(0, protocol, 0).await?,
                    "{protocol:?} subtree roots must agree"
                );
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_tree_state() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let latest = fetch.get_latest_tree_state().await?;
            assert_eq!(
                latest,
                state.get_latest_tree_state().await?,
                "latest tree state must agree"
            );
            assert_eq!(
                latest.height,
                u64::from(tip),
                "latest tree state must be at the fixture tip"
            );
            assert_eq!(
                latest.hash, cache.manifest.tip_hash,
                "latest tree state must be at the fixture tip hash"
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_full() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let (start, end) = seam_band(&cache);
            let all_pools: Vec<i32> = [
                Pool::Transparent,
                Pool::Sapling,
                Pool::Orchard,
                Pool::Ironwood,
            ]
            .into_iter()
            .map(Pool::wire_code)
            .collect();
            let blocks = fetch
                .get_block_range_with_pools(start, end, all_pools.clone())
                .await?;
            assert_eq!(
                blocks,
                state
                    .get_block_range_with_pools(start, end, all_pools)
                    .await?,
                "block range [{start},{end}] must agree"
            );
            assert_eq!(
                blocks.len(),
                usize::try_from(end - start + 1)?,
                "block range [{start},{end}] must be complete"
            );
            for block in &blocks {
                let height = u32::try_from(block.height)?;
                for event in cache.manifest.events_at(height) {
                    let txid = hex_reversed(&event.txid)?;
                    assert!(
                        block.vtx.iter().any(|tx| tx.txid == txid),
                        "block {height} must carry the recorded {} {}",
                        event.kind,
                        event.txid
                    );
                }
            }
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_nullifiers() -> Result<()> {
            let cache = ChainCache::ensure().await?;
            let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
            let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
            let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

            let tip = cache.manifest.fixture.height;
            fetch.wait_for_block(tip, READY).await?;
            state.wait_for_block(tip, READY).await?;
            let (start, end) = seam_band(&cache);
            // TODO(#1088): replace deprecated nullifier-range client usage.
            let blocks = fetch.get_block_range_nullifiers(start, end).await?;
            assert_eq!(
                blocks,
                state.get_block_range_nullifiers(start, end).await?,
                "block range nullifiers [{start},{end}] must agree"
            );
            assert_eq!(
                blocks.len(),
                usize::try_from(end - start + 1)?,
                "block range nullifiers [{start},{end}] must be complete"
            );
            Ok(())
        }
    }
}

/// A display-order txid as the compact block's internal byte order.
fn hex_reversed(txid: &str) -> Result<Vec<u8>> {
    let mut bytes = zaino_testutils::hex::decode(txid, "manifest txid")?;
    bytes.reverse();
    Ok(bytes)
}
