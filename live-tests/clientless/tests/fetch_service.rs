//! These tests compare the output of `FetchService` with the output of `JsonRpcConnector`.

use std::time::Duration;

use anyhow::{Context, Result};
use clientless::rpc::z_validate_address::run_z_validate_for;
use serde_json::{json, Value};
use zaino_testutils::{
    assert_json_equal_ignoring, assert_rpc_parity, Backend, ChainCache, ShieldedProtocol,
    Validator as _, Zainod, Zebrad,
};

const READY: Duration = Duration::from_secs(60);

/// zebra's first regtest halving: `height_for_halving(1)` with the regtest
/// pre-Blossom interval (144), slow start 0 and Blossom at 1.
const FIRST_HALVING_HEIGHT: u32 = 287;

mod launch {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn regtest_no_cache() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let info = indexer.indexer_info().await?;
        assert!(
            !info.chain_name.is_empty(),
            "indexer chain_name must be set: {info:?}"
        );
        Ok(())
    }
}

mod validation {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn validate_address() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let irpc = indexer.json_rpc();
        for (addr, is_script) in [
            ("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma", false),
            ("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2", true),
        ] {
            let i = irpc.call_value("validateaddress", json!([addr])).await?;
            assert_eq!(
                i.get("isvalid").and_then(Value::as_bool),
                Some(true),
                "{addr} isvalid"
            );
            assert_eq!(
                i.get("address").and_then(Value::as_str),
                Some(addr),
                "{addr} address echo"
            );
            assert_eq!(
                i.get("isscript").and_then(Value::as_bool),
                Some(is_script),
                "{addr} isscript"
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    pub(crate) async fn z_validate_address() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let irpc = indexer.json_rpc();
        run_z_validate_for(&irpc).await
    }
}

mod get {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_raw() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _block = indexer
            .json_rpc()
            .call_value("getblock", json!(["1", 0]))
            .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_object() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _block = indexer
            .json_rpc()
            .call_value("getblock", json!(["1", 1]))
            .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn latest_block() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let indexer_tip = indexer.latest_block_height().await?;
        let validator_tip = zebrad.chain_height().await?;
        assert_eq!(
            indexer_tip, validator_tip,
            "indexer tip ({indexer_tip}) must equal validator tip ({validator_tip})"
        );
        assert_eq!(indexer_tip, tip, "indexer tip must be the fixture's tip");

        let indexer_best_hash = indexer
            .json_rpc()
            .call_value("getbestblockhash", json!([]))
            .await?;
        let validator_best_hash = zebrad
            .rpc()
            .call_value("getbestblockhash", json!([]))
            .await?;
        assert_eq!(
            indexer_best_hash, validator_best_hash,
            "indexer best block hash must equal validator best block hash"
        );
        assert_eq!(
            indexer_best_hash.as_str(),
            Some(cache.manifest.tip_hash.as_str()),
            "indexer best block hash must be the fixture's tip hash"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let by_height = indexer.get_block(1).await?;
        assert_eq!(by_height.height, 1, "get_block(1).height");
        assert_eq!(by_height.hash.len(), 32, "block hash must be 32 bytes");
        let hash_bytes: [u8; 32] = by_height
            .hash
            .clone()
            .try_into()
            .ok()
            .context("block hash must be 32 bytes")?;
        let by_hash = indexer.get_block_by_hash(hash_bytes).await?;
        assert_eq!(
            by_height.height, by_hash.height,
            "by-hash height round-trip"
        );
        assert_eq!(by_height.hash, by_hash.hash, "by-hash hash round-trip");
        Ok(())
    }

    /// Parity at genesis and the first blocks, at every recorded transaction's
    /// height (deep in the finalised band, across the seam, at the top) and at
    /// the tip.
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_header() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let heights = (0..10)
            .chain(cache.manifest.events.iter().map(|event| event.height))
            .chain([tip]);
        for height in heights {
            let blk = vrpc
                .call_value("getblock", json!([height.to_string(), 1]))
                .await?;
            let hash = blk
                .get("hash")
                .and_then(Value::as_str)
                .with_context(|| format!("getblock({height},1).hash missing"))?
                .to_string();
            for verbose in [false, true] {
                let params = format!(r#"["{hash}", {verbose}]"#);
                assert_rpc_parity("getblockheader", &params, vrpc, &irpc, &[]).await?;
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn difficulty() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        assert_rpc_parity("getdifficulty", "", vrpc, &irpc, &[]).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn mining_info() -> Result<()> {
        let ignore: &[&str] = &[
            "difficulty",
            "errors",
            "errorstimestamp",
            "genproclimit",
            "localsolps",
            "pooledtx",
            "generate",
        ];

        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let v = zebrad.rpc().call_value("getmininginfo", json!([])).await?;
        let i = indexer
            .json_rpc()
            .call_value("getmininginfo", json!([]))
            .await?;
        assert_json_equal_ignoring("getmininginfo", v, i, ignore);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn peer_info() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        assert_rpc_parity("getpeerinfo", "", vrpc, &irpc, &[]).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_subsidy() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        // getblocksubsidy is a pure function of (height, network schedule).
        // NU7 is active from height 2 here, so issuance is smooth rather than
        // halving: the sweep covers the pre-NU7 block, the activation, the old
        // halving height, every recorded transaction and the tip.
        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let activation = vrpc.call_value("getblocksubsidy", json!([2])).await?;
        let later = vrpc.call_value("getblocksubsidy", json!([tip])).await?;
        anyhow::ensure!(
            activation["miner"] != later["miner"],
            "premise: the subsidy must change between NU7 activation and the tip: \
             {activation} vs {later}"
        );
        let mut heights: Vec<u32> = (0..=10)
            .chain(FIRST_HALVING_HEIGHT - 1..=FIRST_HALVING_HEIGHT + 1)
            .chain(cache.manifest.events.iter().map(|event| event.height))
            .chain([tip])
            .collect();
        heights.sort_unstable();
        heights.dedup();
        for height in heights {
            let params = format!("[{height}]");
            assert_rpc_parity("getblocksubsidy", &params, vrpc, &irpc, &[]).await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn best_blockhash() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let irpc = indexer.json_rpc();
        let block = irpc
            .call_value("getblock", json!([tip.to_string(), 1]))
            .await?;
        let block_hash = block
            .get("hash")
            .and_then(Value::as_str)
            .context("getblock.hash missing")?;
        let best_hash = irpc
            .call_value("getbestblockhash", json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returned non-string")?
            .to_string();
        assert_eq!(
            block_hash, best_hash,
            "getblock(tip).hash must equal getbestblockhash"
        );
        assert_eq!(
            best_hash, cache.manifest.tip_hash,
            "getbestblockhash must be the fixture's tip hash"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_count() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let count = assert_rpc_parity("getblockcount", "", vrpc, &irpc, &[]).await?;
        assert_eq!(
            count.as_u64(),
            Some(u64::from(tip)),
            "getblockcount must equal the fixture's tip"
        );
        Ok(())
    }

    /// At the first recorded shield, a block carrying a transaction with spends.
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_nullifiers() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let shield = cache
            .manifest
            .events
            .iter()
            .find(|event| event.kind == "shield")
            .context("the fixture records a shield")?;
        let cb = indexer.get_block_nullifiers(shield.height).await?;
        assert_eq!(
            cb.height,
            u64::from(shield.height),
            "GetBlockNullifiers must return the requested height"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_range() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _blocks = indexer.get_block_range(1, 10).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_range_nullifiers() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _blocks = indexer.get_block_range_nullifiers(1, 10).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn tree_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let chain_tip = zebrad.chain_height().await?;
        let _ts = indexer.get_tree_state(chain_tip).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn latest_tree_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _ts = indexer.get_latest_tree_state().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn subtree_roots() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _roots = indexer
            .get_subtree_roots(0, ShieldedProtocol::Sapling, 0)
            .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn lightd_info() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let _info = indexer.indexer_info().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn get_network_sol_ps() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        assert_rpc_parity("getnetworksolps", "", vrpc, &irpc, &[]).await?;
        Ok(())
    }
}
