//! Wallet-to-validator end-to-end tests: the fixture's faucet and recipient
//! wallets drive an in-process zainod over its gRPC / JSON-RPC surface.
//!
//! Covered (the full query/send surface):
//! - sends to each pool, `send_to_all`, shielding, mining-reward receipt;
//! - `get_transaction` (mined / mempool), `get_raw_transaction`;
//! - mempool: `get_raw_mempool`, `get_mempool_tx`, `get_mempool_stream`,
//!   `get_mempool_info`;
//! - address queries: `get_address_tx_ids`, `get_address_utxos`,
//!   `get_address_balance`, the `get_taddress_*` family (recipient and
//!   faucet-coinbase variants), `get_address_transactions_regtest`;
//! - tree state: `z_get_treestate`, `z_get_subtrees_by_index`;
//! - block range: default/all pools and the out-of-range edge cases;
//! - compact-block transparent data;
//! - `getblockdeltas` spend-resolution (AP-03 / Zellic #48500) and the
//!   coinbase-only edge case;
//! - `getaddressdeltas` across its five request shapes;
//! - `connect_to_node_get_info` (wallet `get_info` smoke).
//!
//! Dual `*_fetch_vs_state` tests compare zaino's two ingest paths. `Backend::Fetch`
//! (`Rpc`) reaches the validator over JSON-RPC alone; `Backend::State` (`Direct`)
//! additionally runs a zebra `ReadStateService` over the validator's state directory,
//! and the routing table prefers it for every query it can answer. Both arms build the
//! same chain index through one `NodeBackedIndexerService`, so the axis under test is
//! the transport, not the index.

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::{json, Value};
use zaino_testutils::{
    assert_rpc_parity, wait_for_finalised, Backend, ChainCache, Event, Manifest, Party, Pool,
    ShieldedProtocol, TxId, Validator as _, Wallet as _, Zainod, Zebrad, FILLER_ADDRESS,
};

use e2e::{assert_pool_absent, assert_pool_present};

/// Indexer sync / readiness timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;

/// The fixture's faucet → recipient sends that landed in `pool`, lowest first.
fn receipts(manifest: &Manifest, pool: Pool) -> Vec<&Event> {
    manifest
        .events
        .iter()
        .filter(|event| {
            event.kind == "send"
                && event.from == Party::Faucet
                && event.to == Party::Recipient
                && event.pool == pool
        })
        .collect()
}

/// The fixture's shields of the recipient's transparent receipts, lowest first.
fn shields(manifest: &Manifest) -> Vec<&Event> {
    manifest
        .events
        .iter()
        .filter(|event| event.kind == "shield")
        .collect()
}

/// A manifest txid (display-order hex) as the wire's `TxId`.
fn txid(hex: &str) -> Result<TxId> {
    TxId::from_hex(hex).with_context(|| format!("manifest txid `{hex}` is not 64 hex digits"))
}

/// The strings of a JSON-RPC txid array (`getaddresstxids`, `getrawmempool`).
fn txids(reply: &Value) -> Vec<String> {
    reply
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The height span of the coinbase utxos the validator reports for `taddr`:
/// the faucet's matured transparent coinbase, one per block.
async fn coinbase_span(zebrad: &Zebrad, taddr: &str) -> Result<(u32, u32)> {
    let utxos = zebrad
        .rpc()
        .call_value("getaddressutxos", json!([{ "addresses": [taddr] }]))
        .await?;
    let heights: Vec<u64> = utxos
        .as_array()
        .context("getaddressutxos answers an array")?
        .iter()
        .filter_map(|utxo| utxo["height"].as_u64())
        .collect();
    let start = u32::try_from(
        *heights
            .iter()
            .min()
            .context("the faucet taddr holds coinbase")?,
    )?;
    let end = u32::try_from(
        *heights
            .iter()
            .max()
            .context("the faucet taddr holds coinbase")?,
    )?;
    anyhow::ensure!(
        heights.len() == usize::try_from(end - start + 1)?,
        "the faucet's coinbase utxos are one per block over {start}..={end}: {heights:?}"
    );
    Ok((start, end))
}

/// Wallet-driven flows.
mod wallet {
    use super::*;

    /// The faucet's synced wallet holds the Ironwood coinbase the manifest recorded.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn receives_mining_reward(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;

        let balances = faucet.balances().await?;
        assert!(
            balances.get(Pool::Ironwood) > 0,
            "faucet must hold spendable Ironwood coinbase notes, got {balances:?}"
        );
        assert_eq!(
            balances.get(Pool::Ironwood),
            cache.manifest.faucet.balances.ironwood,
            "the synced faucet must see the Ironwood balance the manifest recorded"
        );
        Ok(())
    }

    /// Smoke: faucet and recipient wallets connect and sync without error,
    /// and the indexer reports node info at the fixture's tip.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_node_get_info(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let _faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let _recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let info = indexer.indexer_info().await?;
        assert!(!info.chain_name.is_empty());
        assert_eq!(
            info.block_height,
            u64::from(cache.manifest.fixture.height),
            "the indexer serves the fixture's tip"
        );
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's unified address, whose Orchard
    /// receiver credits Ironwood from NU6.3 (Orchard is spend-locked) and Orchard
    /// before it.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_unified(#[case] backend: Backend) -> Result<()> {
        let credited = Pool::Ironwood;
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        let addr = recipient.address(credited).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(credited),
            before.get(credited) + SEND_AMOUNT,
            "recipient {credited:?} balance must grow by the send"
        );
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's sapling address; the
    /// recipient's synced wallet shows it.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_sapling(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        let addr = recipient.address(Pool::Sapling).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(Pool::Sapling),
            before.get(Pool::Sapling) + SEND_AMOUNT,
            "recipient Sapling balance must grow by the send"
        );
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's transparent address; the
    /// recipient's synced wallet shows it.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        let addr = recipient.address(Pool::Transparent).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(Pool::Transparent),
            before.get(Pool::Transparent) + SEND_AMOUNT,
            "recipient Transparent balance must grow by the send"
        );
        Ok(())
    }

    /// One faucet funds a send to all three pools; each recipient pool
    /// grows by 250_000.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_all(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        // NU6.3: the unified-address (Orchard-receiver) output routes to Ironwood.
        for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
            let addr = recipient.address(pool).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
        }
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;

        let balances = recipient.balances().await?;
        assert_eq!(
            balances.get(Pool::Ironwood),
            before.get(Pool::Ironwood) + SEND_AMOUNT
        );
        // From NU6.3 the unified-address output routes to Ironwood; the
        // orchard pool must stay empty (a nonzero orchard here means the
        // receipt was mislabelled, not merely misrouted).
        assert_eq!(balances.get(Pool::Orchard), 0);
        assert_eq!(
            balances.get(Pool::Sapling),
            before.get(Pool::Sapling) + SEND_AMOUNT
        );
        assert_eq!(
            balances.get(Pool::Transparent),
            before.get(Pool::Transparent) + SEND_AMOUNT
        );
        Ok(())
    }

    /// The recipient receives a transparent 250_000, shields it, and gains
    /// 250_000 − 15_000 fee in the pool the chain's latest activation shields into.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_for_validator(#[case] backend: Backend) -> Result<()> {
        let credited = Pool::Ironwood;
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        // The fee below is for one transparent input, so the send must be the only utxo.
        anyhow::ensure!(
            before.get(Pool::Transparent) == 0,
            "the fixture's recipient holds no transparent funds: {before:?}"
        );
        let taddr = recipient.address(Pool::Transparent).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        let funded = recipient.balances().await?;
        assert_eq!(funded.get(Pool::Transparent), SEND_AMOUNT);

        recipient.shield().await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        let shielded = recipient.balances().await?;
        assert_eq!(
            shielded.get(credited),
            funded.get(credited) + SEND_AMOUNT - SHIELD_FEE,
            "shielded balance must grow by the send net of the ZIP-317 fee, in {credited:?}"
        );
        assert_eq!(
            shielded.get(Pool::Transparent),
            0,
            "shielding sweeps every transparent utxo"
        );
        Ok(())
    }

    /// The recipient's transparent receipts are served from both sides of the
    /// finalisation seam — the lowest from the finalised store, the highest from
    /// the chain head — each identically to the validator's own answer.
    ///
    /// `wait_for_finalised` is the loud check that the lowest receipt really is
    /// in the finalised store; the highest must not be, or both reads come from
    /// the same side.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent_finalization(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;
        let taddr = cache.manifest.recipient.transparent.clone();
        let tip = cache.manifest.fixture.height;

        let receipts = receipts(&cache.manifest, Pool::Transparent);
        let below = receipts
            .first()
            .context("a transparent receipt in the fixture")?;
        let above = receipts
            .last()
            .context("a transparent receipt in the fixture")?;
        let finalised = wait_for_finalised(&indexer, below.height, READY).await?;
        anyhow::ensure!(
            finalised < above.height,
            "the finalised store at {finalised} holds the receipt at {}; both reads would come \
             from the finalised side",
            above.height
        );

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        for receipt in [below, above] {
            let params = json!([{
                "addresses": [&taddr],
                "start": receipt.height,
                "end": receipt.height,
            }]);
            let served = txids(&irpc.call_value("getaddresstxids", params.clone()).await?);
            assert_eq!(
                served,
                vec![receipt.txid.clone()],
                "the receipt at {} must be served",
                receipt.height
            );
            assert_eq!(
                served,
                txids(&vrpc.call_value("getaddresstxids", params).await?),
                "the address txids at {} must be identical to the validator's",
                receipt.height
            );
        }

        let whole = json!([{ "addresses": [&taddr], "start": 1, "end": tip }]);
        let served = txids(&irpc.call_value("getaddresstxids", whole.clone()).await?);
        for receipt in &receipts {
            assert!(
                served.contains(&receipt.txid),
                "the receipt {} at {} must be in the whole-chain answer: {served:?}",
                receipt.txid,
                receipt.height
            );
        }
        assert_eq!(
            served,
            txids(&vrpc.call_value("getaddresstxids", whole).await?),
            "the whole-chain address txids must be identical across the finalisation seam"
        );

        assert_eq!(
            recipient.balances().await?,
            cache.manifest.recipient.balances,
            "the wallet synced through the indexer must see the manifest's balances"
        );
        Ok(())
    }

    /// The indexer serves `get_transaction` for every transaction the fixture
    /// recorded, at its height and with the validator's bytes.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mined(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;

        for event in &cache.manifest.events {
            let served = indexer.get_transaction(&txid(&event.txid)?).await?;
            // A mined transaction reports its block height, not the mempool sentinel 0.
            assert_eq!(
                served.height,
                u64::from(event.height),
                "{} is mined at {}",
                event.txid,
                event.height
            );
            let raw = zebrad
                .rpc()
                .call_value("getrawtransaction", json!([&event.txid, 0]))
                .await?;
            assert_eq!(
                zaino_testutils::hex::encode(&served.data),
                raw.as_str()
                    .context("getrawtransaction returns a hex string")?,
                "zaino must serve the bytes the validator holds for {}",
                event.txid
            );
        }
        Ok(())
    }

    /// The indexer's `getrawmempool` holds exactly the transactions the
    /// validator's does. The txid set order is not contractual, so both
    /// sides are sorted; the two broadcast txids keep `[] == []` from
    /// passing.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?;
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        assert_eq!(
            mempool.len(),
            want.len(),
            "the mirror must hold the two broadcast txs and nothing else: {mempool:?}"
        );
        Ok(())
    }

    /// `get_mempool_tx` returns the two unmined transactions, and the
    /// exclude-by-txid-suffix filter drops one.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_tx() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?;
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?;

        // The validator fixes what the mempool holds before GetMempoolTx is
        // asked; without it a count assertion only says zaino agrees with itself.
        let want = [t_txid.to_string(), u_txid.to_string()];
        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(
            mempool.len(),
            2,
            "the validator must hold exactly the two broadcast txs: {mempool:?}"
        );

        let mut want = [t_txid.as_ref().to_vec(), u_txid.as_ref().to_vec()];
        want.sort();

        let mut all = indexer.get_mempool_tx(Vec::new()).await?;
        all.sort_by_key(|tx| tx.txid.clone());
        assert_eq!(all.len(), 2, "both unmined txs must be present");
        assert_eq!(all[0].txid, want[0]);
        assert_eq!(all[1].txid, want[1]);

        // Excluding the first by its txid suffix leaves only the second.
        let remaining = indexer.get_mempool_tx(vec![want[0][8..].to_vec()]).await?;
        assert_eq!(remaining.len(), 1, "excluding one leaves the other");
        assert_eq!(remaining[0].txid, want[1]);
        Ok(())
    }

    /// The stream serves exactly the transaction bytes the validator holds
    /// unmined.
    ///
    /// GetMempoolStream is bound to the chain tip the request was admitted
    /// against and ends when that tip moves, so the drain can only complete
    /// once a block is mined — hence the drain-while-mining shape. The
    /// per-transaction bytes come from the validator's `getrawtransaction`,
    /// so this catches a mirror that is missing, over-full, or serving the
    /// wrong bytes; `!is_empty()` alone caught none of those.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?;
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        // The validator's own bytes for each mirrored txid: the independent oracle for
        // what zaino streams.
        let mut expected = Vec::with_capacity(mempool.len());
        for txid in &mempool {
            expected.push(
                vrpc.call_value("getrawtransaction", json!([txid]))
                    .await?
                    .as_str()
                    .with_context(|| format!("getrawtransaction {txid} returns hex"))?
                    .to_string(),
            );
        }
        expected.sort();

        // Mining before the subscription is admitted leaves the drain waiting
        // on the *next* tip change, by which point the mempool is empty.
        // Nothing observable reports that the stream is open, so this is a
        // grace period rather than a handshake.
        let (streamed, mined) = tokio::join!(indexer.get_mempool_stream(), async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
            indexer.wait_for_block(tip, READY).await
        });
        mined?;
        let txs = streamed?;

        let mut streamed: Vec<String> = txs
            .iter()
            .map(|tx| zaino_testutils::hex::encode(&tx.data))
            .collect();
        streamed.sort();
        assert_eq!(
            streamed, expected,
            "the mempool stream must serve the validator's unmined transactions verbatim"
        );
        assert!(
            txs.iter().all(|tx| tx.height == 0),
            "unmined transactions carry the height-0 mempool sentinel"
        );
        Ok(())
    }

    /// `getmempoolinfo` agrees with the **validator's own** `getmempoolinfo`.
    ///
    /// `size` and `bytes` are the same quantities on both sides (tx count,
    /// Σ serialized sizes), so they are compared directly — that is the
    /// cross-check that zaino's mirror holds the transactions the validator
    /// does, rather than that zaino agrees with itself. `usage` is not:
    /// zaino reports the ZIP-401 cost total, each transaction floored at
    /// `max(size, 10_000)` because that is the figure its own memory bound
    /// is enforced against, while zebra reports its own memory estimate.
    /// The floor makes `usage >= bytes` the only contractual relation.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_info(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?;
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?;

        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        let validator_info = vrpc.call_value("getmempoolinfo", json!([])).await?;
        let indexer_info = irpc.call_value("getmempoolinfo", json!([])).await?;
        // No block is mined and no send is in flight here, but re-reading the
        // set proves the totals were taken over one quiescent mempool.
        anyhow::ensure!(
            {
                let mut txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                txids.sort();
                txids
            } == mempool,
            "the mempool moved across the two getmempoolinfo reads; totals are \
             not comparable"
        );

        let size = indexer_info
            .get("size")
            .and_then(Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `size`: {indexer_info}")
            })?;
        let bytes = indexer_info
            .get("bytes")
            .and_then(Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `bytes`: {indexer_info}")
            })?;
        let usage = indexer_info
            .get("usage")
            .and_then(Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `usage`: {indexer_info}")
            })?;

        assert_eq!(
            size,
            mempool.len() as u64,
            "size must equal the mirrored txid count"
        );
        assert_eq!(
            size,
            validator_info
                .get("size")
                .and_then(Value::as_u64)
                .with_context(|| format!(
                    "getmempoolinfo must report a numeric `size`: {validator_info}"
                ))?,
            "zaino's mempool holds a different number of transactions than the \
             validator's"
        );
        assert_eq!(
            bytes,
            validator_info
                .get("bytes")
                .and_then(Value::as_u64)
                .with_context(|| format!(
                    "getmempoolinfo must report a numeric `bytes`: {validator_info}"
                ))?,
            "zaino's serialized-byte total disagrees with the validator's"
        );
        assert!(bytes > 0, "two unmined transactions cannot weigh nothing");
        assert!(
            usage >= bytes,
            "usage must be at least bytes: {indexer_info}"
        );
        Ok(())
    }

    /// Broadcast two unmined sends, observe them in zaino's mirror of the
    /// validator's mempool, then mine them in and confirm the balances. The
    /// *unconfirmed* (mempool) pool-balance split under test cannot be
    /// asserted — the `Wallet` port exposes no pending/unconfirmed
    /// pool-balance accessor — so the confirmed balances stand in.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_unverified_mempool(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let before = recipient.balances().await?;
        let ua = recipient.address(Pool::Ironwood).await?;
        let zaddr = recipient.address(Pool::Sapling).await?;
        let ua_txid = faucet.send(&ua, SEND_AMOUNT).await?;
        let sapling_txid = faucet.send(&zaddr, SEND_AMOUNT).await?;

        let want = [ua_txid.to_string(), sapling_txid.to_string()];
        let vrpc = zebrad.rpc();
        let irpc = indexer.json_rpc();
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(
            mempool.len(),
            2,
            "the mirror must hold the two broadcast txs and nothing else: {mempool:?}"
        );

        // `PoolBalances` is confirmed-only, so an unmined send must credit
        // nothing however visible it is in the mempool mirror above.
        recipient.sync().await?;
        let unconfirmed = recipient.balances().await?;
        assert_eq!(
            unconfirmed.get(Pool::Ironwood),
            before.get(Pool::Ironwood),
            "an unmined send must not count toward a confirmed balance"
        );
        assert_eq!(unconfirmed.get(Pool::Sapling), before.get(Pool::Sapling));

        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        let balances = recipient.balances().await?;
        assert_eq!(
            balances.get(Pool::Ironwood),
            before.get(Pool::Ironwood) + SEND_AMOUNT
        );
        assert_eq!(
            balances.get(Pool::Sapling),
            before.get(Pool::Sapling) + SEND_AMOUNT
        );
        Ok(())
    }

    /// `getaddresstxids` over the recipient's taddr around each recorded
    /// receipt returns exactly that receipt, identically to the validator's own.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let taddr = &cache.manifest.recipient.transparent;

        for receipt in receipts(&cache.manifest, Pool::Transparent) {
            let params = json!([{
                "addresses": [taddr],
                "start": receipt.height.saturating_sub(2),
                "end": receipt.height,
            }]);
            let validator_txids = zebrad
                .rpc()
                .call_value("getaddresstxids", params.clone())
                .await?;
            let indexer_txids = indexer
                .json_rpc()
                .call_value("getaddresstxids", params)
                .await?;
            assert_eq!(
                txids(&validator_txids),
                vec![receipt.txid.clone()],
                "getaddresstxids around {} must be the receipt alone, got {validator_txids}",
                receipt.height
            );
            assert_eq!(validator_txids, indexer_txids);
        }
        Ok(())
    }

    /// `getaddressutxos` over the faucet's coinbase taddr returns the utxo set
    /// the validator reports, over both the gRPC and the JSON-RPC surface.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let taddr = cache.manifest.faucet.transparent.clone();

        let params = json!([{ "addresses": [&taddr] }]);
        let validator_utxos = zebrad
            .rpc()
            .call_value("getaddressutxos", params.clone())
            .await?;
        let indexer_utxos = indexer
            .json_rpc()
            .call_value("getaddressutxos", params)
            .await?;
        let want = validator_utxos
            .as_array()
            .context("getaddressutxos answers an array")?;
        assert!(
            !want.is_empty(),
            "the faucet taddr holds matured coinbase: {validator_utxos}"
        );
        assert_eq!(indexer_utxos, validator_utxos);

        let utxos = indexer.get_address_utxos(vec![taddr.clone()], 0, 0).await?;
        assert_eq!(utxos.len(), want.len(), "gRPC serves the same utxo set");
        for (utxo, json) in utxos.iter().zip(want) {
            let expected = txid(json["txid"].as_str().context("utxo txid")?)?;
            assert_eq!(
                utxo.txid,
                expected.as_ref().to_vec(),
                "utxo txid must match the validator's: {json}"
            );
            assert_eq!(utxo.address, taddr);
        }
        Ok(())
    }

    /// The tree state at the tip carries the validator's best block hash and a
    /// non-empty tree per shielded protocol, and matches the validator's
    /// `z_gettreestate` field for field, at the tip and below the seam.
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let tip = cache.manifest.fixture.height;
        let tree = indexer.get_tree_state(tip).await?;
        assert_eq!(tree.height, u64::from(tip));
        let best = zebrad
            .rpc()
            .call_value("getbestblockhash", json!([]))
            .await?;
        assert_eq!(
            tree.hash.as_str(),
            best.as_str()
                .context("getbestblockhash returns a hex string")?
        );
        assert_eq!(tree.hash, cache.manifest.tip_hash);
        assert!(!tree.sapling_tree.is_empty(), "sapling tree must be served");
        assert!(!tree.orchard_tree.is_empty(), "orchard tree must be served");

        let below_seam = receipts(&cache.manifest, Pool::Sapling)
            .first()
            .context("a sapling receipt in the fixture")?
            .height;
        for height in [tip, below_seam] {
            assert_rpc_parity(
                "z_gettreestate",
                &format!(r#"["{height}"]"#),
                zebrad.rpc(),
                &indexer.json_rpc(),
                &[],
            )
            .await?;
        }
        Ok(())
    }

    /// No regtest chain completes a subtree, so the roots are empty — and empty
    /// identically to the validator's own `z_getsubtreesbyindex`.
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        // A subtree completes every 2^16 notes, which no regtest chain reaches;
        // a synthesised root would fail this.
        let roots = indexer
            .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
            .await?;
        assert!(roots.is_empty(), "regtest completes no subtree: {roots:?}");

        assert_rpc_parity(
            "z_getsubtreesbyindex",
            r#"["orchard", 0]"#,
            zebrad.rpc(),
            &indexer.json_rpc(),
            &[],
        )
        .await?;
        Ok(())
    }

    /// `getrawtransaction` for every recorded txid reports its confirming
    /// height, identically to the validator's own answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        for event in &cache.manifest.events {
            let served = assert_rpc_parity(
                "getrawtransaction",
                &format!(r#"["{}", 1]"#, event.txid),
                zebrad.rpc(),
                &indexer.json_rpc(),
                &[],
            )
            .await?;
            assert_eq!(
                served.get("txid").and_then(Value::as_str),
                Some(event.txid.as_str())
            );
            assert_eq!(
                served.get("height").and_then(Value::as_u64),
                Some(u64::from(event.height)),
                "a mined transaction reports its block height"
            );

            // `verbose = 0` is the raw hex form, as the validator serves it.
            assert_rpc_parity(
                "getrawtransaction",
                &format!(r#"["{}", 0]"#, event.txid),
                zebrad.rpc(),
                &indexer.json_rpc(),
                &[],
            )
            .await?;
        }
        Ok(())
    }

    /// `get_taddress_txids` over the recipient's taddr and a range around each
    /// recorded receipt serves that receipt's bytes alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let taddr = cache.manifest.recipient.transparent.clone();

        for receipt in receipts(&cache.manifest, Pool::Transparent) {
            let start = receipt.height.saturating_sub(2);
            let txs = indexer
                .get_taddress_txids(taddr.clone(), start, receipt.height)
                .await?;
            assert_eq!(
                txs.len(),
                1,
                "the span [{start}, {}] holds exactly the one receipt",
                receipt.height
            );
            assert_eq!(
                zaino_testutils::hex::encode(&txs[0].data),
                zebrad
                    .rpc()
                    .call_value("getrawtransaction", json!([&receipt.txid, 0]))
                    .await?
                    .as_str()
                    .context("getrawtransaction returns a hex string")?
            );
        }
        Ok(())
    }

    /// `get_address_utxos` over the faucet's coinbase taddr serves each utxo
    /// the validator reports, with its value and height.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let taddr = cache.manifest.faucet.transparent.clone();

        let validator_utxos = zebrad
            .rpc()
            .call_value("getaddressutxos", json!([{ "addresses": [&taddr] }]))
            .await?;
        let want = validator_utxos
            .as_array()
            .context("getaddressutxos answers an array")?;
        assert!(
            !want.is_empty(),
            "the faucet taddr holds matured coinbase: {validator_utxos}"
        );

        let utxos = indexer.get_address_utxos(vec![taddr.clone()], 0, 0).await?;
        assert_eq!(utxos.len(), want.len(), "every coinbase utxo is served");
        for (utxo, json) in utxos.iter().zip(want) {
            assert_eq!(utxo.address, taddr);
            assert_eq!(
                Some(utxo.value_zat),
                json["satoshis"].as_i64(),
                "utxo value must match the validator's: {json}"
            );
            assert_eq!(
                Some(utxo.height),
                json["height"].as_u64(),
                "utxo height must match the validator's: {json}"
            );
            assert_eq!(
                Some(i64::from(utxo.index)),
                json["outputIndex"].as_i64(),
                "utxo index must match the validator's: {json}"
            );
        }
        Ok(())
    }

    /// `get_address_utxos_stream` over the faucet's coinbase taddr serves the
    /// same set as the unary form.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos_stream() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let taddr = cache.manifest.faucet.transparent.clone();

        let streamed = indexer
            .get_address_utxos_stream(vec![taddr.clone()], 0, 0)
            .await?;
        assert!(
            !streamed.is_empty(),
            "the faucet taddr holds matured coinbase"
        );
        let total: u64 = streamed
            .iter()
            .map(|utxo| u64::try_from(utxo.value_zat))
            .sum::<Result<u64, _>>()?;
        assert_eq!(
            total, cache.manifest.faucet.balances.transparent,
            "the streamed utxos sum to the manifest's transparent balance"
        );
        for utxo in &streamed {
            assert_eq!(utxo.address, taddr);
        }
        // The stream and unary forms serve the same set.
        assert_eq!(
            streamed,
            indexer.get_address_utxos(vec![taddr], 0, 0).await?
        );
        Ok(())
    }

    /// `get_transaction` over an unmined orchard send, then over the same
    /// transaction once mined — the mempool-to-mined transition, end to end.
    ///
    /// The invariants, in order:
    /// - an unmined transaction carries the mempool height sentinel (`0`),
    ///   not the current tip height. This is the whole point of the test:
    ///   returning the tip would make an unconfirmed transaction look
    ///   confirmed to every wallet on the other end of the wire.
    /// - the bytes zaino serves are the transaction that was actually
    ///   broadcast. The validator's `getrawtransaction` is the oracle —
    ///   comparing against a hash zaino also computed would only prove
    ///   zaino agrees with itself.
    /// - once mined, the same query reports the real confirmation height,
    ///   not the sentinel.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mempool() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let addr = recipient.address(Pool::Orchard).await?;
        let txid = faucet.send(&addr, SEND_AMOUNT).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let unmined = indexer.get_transaction(&txid).await?;
        assert_eq!(
            unmined.height, 0,
            "an unmined transaction must carry the mempool height sentinel, \
             not the tip height"
        );

        // The validator is the independent oracle for what was broadcast.
        let vrpc = zebrad.rpc();
        let raw_hex = vrpc
            .call_value("getrawtransaction", json!([txid.to_string()]))
            .await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;
        assert_eq!(
            zaino_testutils::hex::encode(&unmined.data),
            raw_hex,
            "get_transaction served different bytes than the validator holds \
             for {txid}"
        );

        // Mine it: the same query must flip off the sentinel onto the real
        // confirmation height.
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;

        let mined = indexer.get_transaction(&txid).await?;
        assert_eq!(
            mined.height,
            u64::from(tip),
            "a mined transaction must report the height of the block that \
             confirmed it, not the mempool sentinel"
        );
        assert_eq!(
            mined.data, unmined.data,
            "mining must not change the transaction bytes zaino serves"
        );
        Ok(())
    }

    /// `getaddressbalance` reports the manifest's transparent balances: the
    /// faucet's matured coinbase, and the recipient's zero (every receipt was
    /// shielded) with `received` summing the receipts — identically to the
    /// validator's own answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let faucet = assert_rpc_parity(
            "getaddressbalance",
            &format!(
                r#"[{{"addresses": ["{}"]}}]"#,
                cache.manifest.faucet.transparent
            ),
            zebrad.rpc(),
            &indexer.json_rpc(),
            &[],
        )
        .await?;
        assert!(
            cache.manifest.faucet.balances.transparent > 0,
            "the fixture's faucet holds transparent coinbase"
        );
        assert_eq!(
            faucet.get("balance").and_then(Value::as_u64),
            Some(cache.manifest.faucet.balances.transparent),
            "getaddressbalance must report the faucet's manifest balance: {faucet}"
        );

        let recipient = assert_rpc_parity(
            "getaddressbalance",
            &format!(
                r#"[{{"addresses": ["{}"]}}]"#,
                cache.manifest.recipient.transparent
            ),
            zebrad.rpc(),
            &indexer.json_rpc(),
            &[],
        )
        .await?;
        assert_eq!(
            recipient.get("balance").and_then(Value::as_u64),
            Some(cache.manifest.recipient.balances.transparent),
            "getaddressbalance must report the recipient's manifest balance: {recipient}"
        );
        let received: u64 = receipts(&cache.manifest, Pool::Transparent)
            .iter()
            .map(|receipt| receipt.amount)
            .sum();
        assert_eq!(
            recipient.get("received").and_then(Value::as_u64),
            Some(received),
            "received must sum the recorded transparent receipts: {recipient}"
        );
        Ok(())
    }

    /// `GetTaddressBalance` over the faucet's coinbase taddr reports the
    /// manifest's transparent balance.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

        let bal = indexer
            .get_taddress_balance(vec![cache.manifest.faucet.transparent.clone()])
            .await?;
        assert!(bal.value_zat > 0, "the faucet taddr holds matured coinbase");
        assert_eq!(
            bal.value_zat,
            i64::try_from(cache.manifest.faucet.balances.transparent)?,
            "get_taddress_balance must report the manifest's transparent balance"
        );
        Ok(())
    }
}

/// One-chain fetch-vs-state comparisons: each test stands up one zebrad, a
/// `Backend::Fetch` zainod and a `Backend::State` zainod inline, and asserts
/// `fetch == state` over their gRPC / JSON-RPC surface.
mod zebrad {
    use super::*;

    /// `get_block_range` with no pools == requesting the shielded pools,
    /// fetch==state, and each recorded Ironwood receipt's block carries that
    /// send after the coinbase with no transparent data.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes. This must equal `PoolTypeFilter::default()` — every
        // shielded pool, ironwood included — or the default-vs-explicit checks below compare
        // two different filters.
        let shielded_pools = vec![2, 3, 4];

        for receipt in receipts(&cache.manifest, Pool::Ironwood) {
            let (start, end) = (receipt.height - 1, receipt.height + 1);

            let fetch_default = fetch.get_block_range(start, end).await?;
            let fetch_shielded = fetch
                .get_block_range_with_pools(start, end, shielded_pools.clone())
                .await?;
            assert_eq!(fetch_default, fetch_shielded);

            let state_shielded = state
                .get_block_range_with_pools(start, end, shielded_pools.clone())
                .await?;
            let state_default = state.get_block_range(start, end).await?;
            assert_eq!(state_default, state_shielded);

            assert_eq!(fetch_default, state_default);

            let compact_block = state_default
                .iter()
                .find(|block| block.height == u64::from(receipt.height))
                .with_context(|| format!("the block at {} in the range", receipt.height))?;
            let txid = txid(&receipt.txid)?;
            let send = compact_block
                .vtx
                .iter()
                .find(|tx| tx.txid == txid.as_ref().to_vec())
                .with_context(|| format!("the receipt {} in its compact block", receipt.txid))?;
            assert_eq!(send.index, 1, "the send follows the coinbase");
            assert_pool_present(compact_block, &txid, Pool::Ironwood);
            for tx in &compact_block.vtx {
                assert!(
                    tx.vin.is_empty(),
                    "transparent data should be absent when no pool types are requested"
                );
                assert!(
                    tx.vout.is_empty(),
                    "transparent data should be absent when no pool types are requested"
                );
            }
        }
        Ok(())
    }

    /// With all pools requested the fetch and state indexers agree, and each
    /// burst's three receipt blocks carry the coinbase plus the send with its
    /// pool data.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];

        let transparent = receipts(&cache.manifest, Pool::Transparent);
        let sapling = receipts(&cache.manifest, Pool::Sapling);
        let ironwood = receipts(&cache.manifest, Pool::Ironwood);
        anyhow::ensure!(
            !transparent.is_empty()
                && transparent.len() == sapling.len()
                && sapling.len() == ironwood.len(),
            "every burst sends into every pool"
        );
        for ((t, s), i) in transparent.iter().zip(&sapling).zip(&ironwood) {
            let (start, end) = (t.height, i.height);
            anyhow::ensure!(
                start < s.height && s.height < end,
                "a burst's receipts are mined in pool order"
            );

            let fetch_range = fetch
                .get_block_range_with_pools(start, end, all_pools.clone())
                .await?;
            let state_range = state
                .get_block_range_with_pools(start, end, all_pools.clone())
                .await?;
            assert_eq!(fetch_range, state_range);

            for (receipt, pool) in [
                (t, Pool::Transparent),
                (s, Pool::Sapling),
                (i, Pool::Ironwood),
            ] {
                let compact_block = state_range
                    .iter()
                    .find(|block| block.height == u64::from(receipt.height))
                    .with_context(|| format!("the block at {} in the range", receipt.height))?;
                // coinbase + the send
                assert_eq!(compact_block.vtx.len(), 2);
                let txid = txid(&receipt.txid)?;
                assert_pool_present(compact_block, &txid, pool);
            }
            // The unified-address send must carry no Orchard actions from NU6.3.
            let ironwood_block = state_range
                .iter()
                .find(|block| block.height == u64::from(i.height))
                .with_context(|| format!("the block at {} in the range", i.height))?;
            assert_pool_absent(ironwood_block, &txid(&i.txid)?, Pool::Orchard);
        }
        Ok(())
    }

    /// The fetch and state indexers agree on the tree state at the tip, and it is the tip's:
    /// height and block hash pin the reply to the block the validator calls the tip, which a
    /// default-valued or stale `TreeState` cannot satisfy. They agree below the seam too.
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let tip = cache.manifest.fixture.height;
        let fetch_tree = fetch.get_tree_state(tip).await?;
        assert_eq!(
            fetch_tree.height,
            u64::from(tip),
            "the tree state must be the tip's"
        );
        assert_eq!(
            fetch_tree.hash, tip_hash,
            "the tree state must name the block the validator calls the tip"
        );
        assert_eq!(fetch_tree, state.get_tree_state(tip).await?);

        let below_seam = receipts(&cache.manifest, Pool::Sapling)
            .first()
            .context("a sapling receipt in the fixture")?
            .height;
        let fetch_tree = fetch.get_tree_state(below_seam).await?;
        assert_eq!(fetch_tree.height, u64::from(below_seam));
        assert_eq!(fetch_tree, state.get_tree_state(below_seam).await?);
        Ok(())
    }

    /// Both indexers report no orchard subtree root. Zebra tracks roots at
    /// `TRACKED_SUBTREE_HEIGHT = 16`, so the first one needs 2^16 note commitments and no
    /// regtest fixture reaches a boundary — the emptiness is the assertion, and a synthesised
    /// root would fail it. Coverage of a real boundary belongs in a `packages/zaino-state`
    /// unit test over the mock chain source.
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let fetch_roots = fetch
            .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
            .await?;
        assert!(
            fetch_roots.is_empty(),
            "a regtest chain completes no 2^16-note subtree, so no root can exist: \
             {fetch_roots:?}"
        );
        assert_eq!(
            fetch_roots,
            state
                .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                .await?,
        );
        Ok(())
    }

    /// The fetch and state indexers serve the same whole `getrawtransaction` reply for every
    /// recorded transaction, and its `hex` is the transaction the validator holds — the
    /// validator is the oracle, so a reply both indexers derived the same wrong way still
    /// fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let vrpc = zebrad.rpc();
        for event in &cache.manifest.events {
            let raw_hex = vrpc
                .call_value("getrawtransaction", json!([&event.txid]))
                .await?;
            let raw_hex = raw_hex
                .as_str()
                .context("getrawtransaction must return a hex string at verbosity 0")?;

            let params = json!([&event.txid, 1]);
            let fetch_tx = fetch
                .json_rpc()
                .call_value("getrawtransaction", params.clone())
                .await?;
            assert_eq!(
                fetch_tx.get("hex").and_then(Value::as_str),
                Some(raw_hex),
                "zaino served different bytes than the validator holds for {}",
                event.txid
            );
            assert_eq!(
                fetch_tx,
                state
                    .json_rpc()
                    .call_value("getrawtransaction", params)
                    .await?,
            );
        }
        Ok(())
    }

    /// `getaddresstxids` over the recipient's taddr returns exactly each recorded receipt at
    /// its height, on both ingest paths. The expected list is the manifest's, so an empty or
    /// over-full answer cannot pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let taddr = &cache.manifest.recipient.transparent;
        for receipt in receipts(&cache.manifest, Pool::Transparent) {
            let height = receipt.height;
            let params = json!([{ "addresses": [taddr], "start": height, "end": height }]);
            let fetch_txids = txids(
                &fetch
                    .json_rpc()
                    .call_value("getaddresstxids", params.clone())
                    .await?,
            );
            assert_eq!(
                fetch_txids,
                vec![receipt.txid.clone()],
                "the receipt is the only transaction touching the recipient taddr at {height}"
            );
            assert_eq!(
                fetch_txids,
                txids(
                    &state
                        .json_rpc()
                        .call_value("getaddresstxids", params)
                        .await?
                )
            );
        }
        Ok(())
    }

    /// `getaddressutxos` over the faucet's coinbase taddr returns the validator's utxo set
    /// summing to the manifest balance, on both ingest paths, over both the JSON-RPC and
    /// gRPC surfaces. Whole replies are compared: a count alone would pass on a wrong value,
    /// height or script.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let taddr = cache.manifest.faucet.transparent.clone();
        let params = json!([{ "addresses": [&taddr] }]);
        let validator_json = zebrad
            .rpc()
            .call_value("getaddressutxos", params.clone())
            .await?;
        let fetch_json = fetch
            .json_rpc()
            .call_value("getaddressutxos", params.clone())
            .await?;
        let count = fetch_json
            .as_array()
            .context("getaddressutxos answers an array")?
            .len();
        let total: u64 = fetch_json
            .as_array()
            .context("getaddressutxos answers an array")?
            .iter()
            .filter_map(|utxo| utxo["satoshis"].as_u64())
            .sum();
        assert_eq!(
            total, cache.manifest.faucet.balances.transparent,
            "the faucet taddr's utxos must sum to the manifest balance: {fetch_json}"
        );
        assert_eq!(
            fetch_json, validator_json,
            "the validator is the oracle for the utxo set"
        );
        assert_eq!(
            fetch_json,
            state
                .json_rpc()
                .call_value("getaddressutxos", params)
                .await?,
        );

        let zero = 0u32;
        let fetch_utxos = fetch
            .get_address_utxos(vec![taddr.clone()], zero, 0)
            .await?;
        assert_eq!(fetch_utxos.len(), count, "gRPC serves the same utxo set");
        assert_eq!(
            fetch_utxos,
            state.get_address_utxos(vec![taddr], zero, 0).await?
        );
        Ok(())
    }

    /// Both indexers mirror the *validator's* mempool while two sends sit unmined.
    ///
    /// Not an ingest-path comparison: the mempool has no read-state implementation at all
    /// (`GetMempoolTxids` is JSON-RPC-only on both backends), so the two indexers would agree
    /// by construction. The validator is the oracle instead, and the two broadcast txids keep
    /// `[] == []` from passing.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &fetch).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &fetch).await?;

        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?;
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?;

        let vrpc = zebrad.rpc();
        let want = [t_txid.to_string(), u_txid.to_string()];
        let frpc = fetch.json_rpc();
        let srpc = state.json_rpc();
        // Zaino's mempool is a polled mirror (500 ms cadence), so each indexer agrees with
        // the validator only eventually; `want` is the non-vacuity probe — a send that was
        // built but never relayed leaves every side empty and equal.
        let deadline = tokio::time::Instant::now() + READY;
        let (fetch_mempool, state_mempool) = loop {
            let mut sets = Vec::new();
            for rpc in [vrpc, &frpc, &srpc] {
                let mut txids = rpc
                    .call_value("getrawmempool", json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                txids.sort();
                sets.push(txids);
            }
            if sets[0] == sets[1] && sets[0] == sets[2] && want.iter().all(|t| sets[0].contains(t))
            {
                break (sets[1].clone(), sets[2].clone());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the indexer mempools never converged on the validator's {sets:?} holding \
                 {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        assert_eq!(
            fetch_mempool.len(),
            want.len(),
            "the mirror must hold the two broadcast txs and nothing else: {fetch_mempool:?}"
        );
        assert_eq!(fetch_mempool, state_mempool);
        Ok(())
    }

    /// `GetTaddressTxids` over the recipient's taddr returns exactly each recorded receipt's
    /// transaction at its height, on both ingest paths. The reply carries whole raw
    /// transactions, so comparing it covers the bytes and not merely the count.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_transactions_regtest() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state indexer must answer and the fetch one must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let taddr = cache.manifest.recipient.transparent.clone();
        for receipt in receipts(&cache.manifest, Pool::Transparent) {
            let height = receipt.height;
            let raw_hex = zebrad
                .rpc()
                .call_value("getrawtransaction", json!([&receipt.txid]))
                .await?;
            let raw_hex = raw_hex
                .as_str()
                .context("getrawtransaction must return a hex string at verbosity 0")?;

            let fetch_txs = fetch
                .get_taddress_txids(taddr.clone(), height, height)
                .await?;
            assert_eq!(
                fetch_txs.len(),
                1,
                "only the receipt pays the recipient taddr in block {height}"
            );
            assert_eq!(
                zaino_testutils::hex::encode(&fetch_txs[0].data),
                raw_hex,
                "the served transaction must be the one the validator holds"
            );
            assert_eq!(
                fetch_txs,
                state
                    .get_taddress_txids(taddr.clone(), height, height)
                    .await?
            );
        }
        Ok(())
    }

    /// Over the faucet's transparent coinbase blocks every compact-block tx carries a
    /// transparent vout, so each vout's `script_pub_key` is non-empty — and both ingest paths
    /// build the same range.
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_data_in_compact_block() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = coinbase_span(&zebrad, &cache.manifest.faucet.transparent).await?;
        let state_range = state
            .get_block_range_with_pools(start, end, all_pools.clone())
            .await?;
        assert_eq!(
            state_range.len(),
            usize::try_from(end - start + 1)?,
            "the range must serve every height in [{start}, {end}]"
        );
        assert_eq!(
            state_range,
            fetch
                .get_block_range_with_pools(start, end, all_pools)
                .await?
        );
        for cb in state_range {
            for tx in cb.vtx {
                assert!(
                    !tx.vout
                        .first()
                        .expect("transparent vout present")
                        .script_pub_key
                        .is_empty(),
                    "each tx's transparent output must carry a script pub key"
                );
            }
        }
        Ok(())
    }

    /// The fetch and state indexers agree on `getaddresstxids` over the faucet's coinbase
    /// taddr. Its coinbase blocks each pay it once, so over four of them the txid count must
    /// be four, which an empty or truncated answer cannot satisfy; the validator's answer is
    /// the oracle for the txids themselves.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids_faucet_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let faucet_taddr = &cache.manifest.faucet.transparent;
        let (start, _) = coinbase_span(&zebrad, faucet_taddr).await?;
        let end = start + 3;
        let params = json!([{ "addresses": [faucet_taddr], "start": start, "end": end }]);
        let validator_txids = txids(
            &zebrad
                .rpc()
                .call_value("getaddresstxids", params.clone())
                .await?,
        );
        assert_eq!(
            validator_txids.len(),
            4,
            "every block in [{start}, {end}] pays one coinbase to the faucet taddr: \
             {validator_txids:?}"
        );
        let fetch_txids = txids(
            &fetch
                .json_rpc()
                .call_value("getaddresstxids", params.clone())
                .await?,
        );
        let state_txids = txids(
            &state
                .json_rpc()
                .call_value("getaddresstxids", params)
                .await?,
        );
        assert_eq!(fetch_txids, validator_txids);
        assert_eq!(fetch_txids, state_txids);
        Ok(())
    }

    /// The fetch and state indexers agree on the transparent balance of the faucet's
    /// coinbase taddr, and that balance is the sum of the utxos each reports for the same
    /// address and the manifest's figure — a cross-check no zero-valued or truncated answer
    /// survives.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance_faucet_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let faucet_taddr = cache.manifest.faucet.transparent.clone();
        let utxos = fetch
            .json_rpc()
            .call_value("getaddressutxos", json!([{ "addresses": [&faucet_taddr] }]))
            .await?;
        let utxo_total: i64 = utxos
            .as_array()
            .context("getaddressutxos must return an array")?
            .iter()
            .filter_map(|u| u.get("satoshis").and_then(Value::as_i64))
            .sum();
        assert!(utxo_total > 0, "faucet taddr must hold coinbase value");
        assert_eq!(
            utxo_total,
            i64::try_from(cache.manifest.faucet.balances.transparent)?,
            "the utxos must sum to the manifest's transparent balance"
        );

        let fetch_bal = fetch
            .get_taddress_balance(vec![faucet_taddr.clone()])
            .await?;
        let state_bal = state.get_taddress_balance(vec![faucet_taddr]).await?;
        assert_eq!(
            fetch_bal.value_zat, utxo_total,
            "the balance must be the sum of the utxos reported for the same address"
        );
        assert_eq!(fetch_bal.value_zat, state_bal.value_zat);
        Ok(())
    }

    /// The fetch and state indexers agree on `GetAddressUtxos` over the faucet's coinbase
    /// taddr. `max_entries = 3` from its first coinbase block, over a span that pays it in
    /// every block, must return exactly three utxos, so a truncated or empty reply cannot
    /// pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_faucet_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let faucet_taddr = cache.manifest.faucet.transparent.clone();
        let (start, _) = coinbase_span(&zebrad, &faucet_taddr).await?;
        let fetch_utxos = fetch
            .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
            .await?;
        assert_eq!(
            fetch_utxos.len(),
            3,
            "every block from {start} pays the faucet taddr, so `max_entries` binds"
        );
        assert_eq!(
            fetch_utxos,
            state
                .get_address_utxos(vec![faucet_taddr], start, 3)
                .await?
        );
        Ok(())
    }

    /// The streamed utxos agree between the fetch and state indexers over the faucet's
    /// coinbase taddr, and with the unary reply over the same arguments.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_stream_faucet_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        let faucet_taddr = cache.manifest.faucet.transparent.clone();
        let (start, _) = coinbase_span(&zebrad, &faucet_taddr).await?;
        let fetch_utxos = fetch
            .get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3)
            .await?;
        assert_eq!(
            fetch_utxos.len(),
            3,
            "every block from {start} pays the faucet taddr, so `max_entries` binds"
        );
        assert_eq!(
            fetch_utxos,
            fetch
                .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
                .await?,
            "the streamed and unary replies must agree over the same arguments"
        );
        assert_eq!(
            fetch_utxos,
            state
                .get_address_utxos_stream(vec![faucet_taddr], start, 3)
                .await?
        );
        Ok(())
    }

    /// The five `getaddressdeltas` request shapes: the bare-address form, the
    /// multi-address union, the `chainInfo` form's named endpoints, an `end` past the
    /// tip clamping down to it, and an address with no history answering empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn address_deltas() -> Result<()> {
        // Valid-format regtest taddr that nothing on this chain ever pays.
        const UNKNOWN_TADDR: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        let faucet_taddr = &cache.manifest.faucet.transparent;
        let recipient_taddr = &cache.manifest.recipient.transparent;
        let tip = cache.manifest.fixture.height;
        // The recipient taddr's history: each burst credits it once and the
        // shield that follows debits it.
        let receipts = receipts(&cache.manifest, Pool::Transparent);
        let shields = shields(&cache.manifest);

        // zebrad implements no `getaddressdeltas`, so only a read-state can answer it. The
        // state indexer must answer and the fetch one must not, or the assertions below are
        // reading the validator over the one transport rather than zaino's read-state.
        let srpc = state.json_rpc();
        let bare = srpc
            .call_value("getaddressdeltas", json!([recipient_taddr]))
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getaddressdeltas", json!([recipient_taddr]))
                .await
                .is_err(),
            "the fetch indexer answered getaddressdeltas, which only a read-state can serve"
        );

        // The bare-address form answers the rangeless whole chain as a plain array.
        let bare = bare
            .as_array()
            .context("the bare-address form answers a delta array")?;
        assert_eq!(
            bare.len(),
            receipts.len() + shields.len(),
            "the recipient taddr's deltas are its receipts and the shields that spent them: \
             {bare:?}"
        );
        for receipt in &receipts {
            let delta = bare
                .iter()
                .find(|delta| delta["height"].as_u64() == Some(u64::from(receipt.height)))
                .with_context(|| format!("the receipt's delta at {}", receipt.height))?;
            assert_eq!(delta["txid"].as_str(), Some(receipt.txid.as_str()));
            assert_eq!(
                delta["index"].as_u64(),
                Some(0),
                "the send's sole transparent output"
            );
            assert_eq!(
                delta["satoshis"].as_i64(),
                Some(i64::try_from(receipt.amount)?)
            );
            assert_eq!(delta["address"].as_str(), Some(recipient_taddr.as_str()));
        }
        for shield in &shields {
            let delta = bare
                .iter()
                .find(|delta| delta["height"].as_u64() == Some(u64::from(shield.height)))
                .with_context(|| format!("the shield's delta at {}", shield.height))?;
            assert_eq!(delta["txid"].as_str(), Some(shield.txid.as_str()));
            assert_eq!(
                delta["satoshis"].as_i64(),
                Some(-i64::try_from(shield.amount)?),
                "the shield debits the receipt it spends"
            );
            assert_eq!(delta["address"].as_str(), Some(recipient_taddr.as_str()));
        }

        let both = srpc
            .call_value(
                "getaddressdeltas",
                json!([{
                    "addresses": [faucet_taddr, recipient_taddr],
                    "start": 0,
                    "end": tip,
                }]),
            )
            .await?;
        let both = both
            .as_array()
            .context("a request without chainInfo answers a delta array")?;
        assert!(
            both.iter()
                .any(|delta| delta["address"].as_str() == Some(faucet_taddr.as_str()))
                && both
                    .iter()
                    .any(|delta| delta["address"].as_str() == Some(recipient_taddr.as_str())),
            "a multi-address request unions every requested address's deltas"
        );

        let named = srpc
            .call_value(
                "getaddressdeltas",
                json!([{
                    "addresses": [faucet_taddr, recipient_taddr],
                    "start": 1,
                    "end": tip,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(named["start"]["height"].as_u64(), Some(1));
        assert_eq!(named["end"]["height"].as_u64(), Some(u64::from(tip)));
        assert!(!named["deltas"]
            .as_array()
            .context("the chainInfo form wraps its deltas")?
            .is_empty());

        let clamped = srpc
            .call_value(
                "getaddressdeltas",
                json!([{
                    "addresses": [faucet_taddr],
                    "start": 1,
                    "end": tip + 100,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(
            clamped["end"]["height"].as_u64(),
            Some(u64::from(tip)),
            "an end past the tip is clamped to the tip, not rejected"
        );

        let unknown = srpc
            .call_value(
                "getaddressdeltas",
                json!([{
                    "addresses": [UNKNOWN_TADDR],
                    "start": 1,
                    "end": tip,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(
            unknown["deltas"],
            json!([]),
            "an address with no history answers the chainInfo shape with no deltas"
        );
        Ok(())
    }

    /// Regression coverage for AP-03 / Zellic #48500
    /// (`get_block_deltas_resolves_transparent_spend`): the State backend used to
    /// silently drop every *non-coinbase transparent spend input* from
    /// `getblockdeltas`, because Zebra's stateless verbosity-2 transaction object
    /// leaves an input's value/address unset. The fix resolves each spend's
    /// prevout and reads the spent output's value/address.
    ///
    /// Each recorded burst pays the recipient a transparent output and then
    /// shields it, producing a non-coinbase transparent input that references the
    /// funding output. The spend block's `InputDelta` must be present and resolve
    /// to the funding output's address and full (negated) value — pre-fix this
    /// input was dropped and `inputs` was empty, so balances overstated funds.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_resolves_transparent_spend() -> Result<()> {
        // State-backend only: zebra serves no `getblockdeltas` RPC, so only the
        // state backend — which synthesizes the deltas from a verbosity-2 block
        // and resolves each spend's prevout via its `ReadStateService` — can
        // answer it. There is no fetch path and no fetch-vs-state cross-check.
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;
        let irpc = indexer.json_rpc();
        let vrpc = zebrad.rpc();
        let recipient_taddr = &cache.manifest.recipient.transparent;

        let receipts = receipts(&cache.manifest, Pool::Transparent);
        let shields = shields(&cache.manifest);
        anyhow::ensure!(
            !receipts.is_empty() && receipts.len() == shields.len(),
            "every transparent receipt is followed by the shield that spends it"
        );
        for (funding, spend) in receipts.iter().zip(&shields) {
            anyhow::ensure!(
                funding.height < spend.height,
                "the shield at {} spends the receipt at {}",
                spend.height,
                funding.height
            );
            let funding_block_hash = vrpc
                .call_value("getblockhash", json!([funding.height]))
                .await?
                .as_str()
                .context("getblockhash returns a hash string")?
                .to_string();
            let spend_block_hash = vrpc
                .call_value("getblockhash", json!([spend.height]))
                .await?
                .as_str()
                .context("getblockhash returns a hash string")?
                .to_string();

            // Locate the funding output by its txid and capture its index / address.
            let funding_deltas = irpc
                .call_value("getblockdeltas", json!([funding_block_hash]))
                .await?;
            let funding_delta = funding_deltas["deltas"]
                .as_array()
                .context("getblockdeltas answers a deltas array")?
                .iter()
                .find(|delta| delta["txid"].as_str() == Some(funding.txid.as_str()))
                .with_context(|| format!("the funding tx {} in its block", funding.txid))?;
            let funding_output = funding_delta["outputs"]
                .as_array()
                .context("a delta carries its outputs")?
                .iter()
                .find(|output| output["address"].as_str() == Some(recipient_taddr.as_str()))
                .with_context(|| format!("the output paying the recipient in {}", funding.txid))?;
            assert_eq!(
                funding_output["satoshis"].as_i64(),
                Some(i64::try_from(funding.amount)?),
                "the funding output carries the recorded amount"
            );
            let funding_vout = funding_output["index"].as_u64().context("output index")?;

            // The spend's input must be present and resolved to the funding output's
            // address and full value (negative — it is a debit). Pre-fix `inputs` was
            // empty and this lookup would fail.
            let spend_deltas = irpc
                .call_value("getblockdeltas", json!([spend_block_hash]))
                .await?;
            let input = spend_deltas["deltas"]
                .as_array()
                .context("getblockdeltas answers a deltas array")?
                .iter()
                .flat_map(|delta| delta["inputs"].as_array().cloned().unwrap_or_default())
                .find(|input| {
                    input["prevtxid"].as_str() == Some(funding.txid.as_str())
                        && input["prevout"].as_u64() == Some(funding_vout)
                })
                .with_context(|| {
                    format!(
                        "the spend input referencing {}:{funding_vout} at {}",
                        funding.txid, spend.height
                    )
                })?;

            assert_eq!(
                input["address"].as_str(),
                Some(recipient_taddr.as_str()),
                "input must resolve to the prevout's address"
            );
            assert_eq!(
                input["satoshis"].as_i64(),
                Some(-i64::try_from(funding.amount)?),
                "input must resolve to the prevout's full value, negated"
            );
        }
        Ok(())
    }

    /// The fixture's tip block carries only its filler coinbase transaction —
    /// the coinbase input is skipped and `getblockdeltas` fabricates no
    /// transparent input deltas.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_coinbase_only_block_has_no_inputs() -> Result<()> {
        // State-backend only (cf. `get_block_deltas_resolves_transparent_spend`):
        // zebra serves no `getblockdeltas` RPC, so only the synthesizing state
        // backend can answer it.
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;
        let irpc = indexer.json_rpc();

        let tip = cache.manifest.fixture.height;
        anyhow::ensure!(
            cache.manifest.events_at(tip).next().is_none(),
            "the fixture's tip block holds only filler coinbase"
        );
        let block_hash = irpc
            .call_value("getbestblockhash", json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        assert_eq!(block_hash, cache.manifest.tip_hash);

        let deltas = irpc
            .call_value("getblockdeltas", json!([block_hash]))
            .await?;
        let deltas = deltas["deltas"]
            .as_array()
            .context("getblockdeltas answers a deltas array")?;
        assert_eq!(
            deltas.len(),
            1,
            "a coinbase-only block has one transaction delta: {deltas:?}"
        );
        let all_empty = deltas.iter().all(|d| {
            d.get("inputs")
                .and_then(Value::as_array)
                .is_none_or(|inputs| inputs.is_empty())
        });
        assert!(all_empty, "a coinbase-only block must have no input deltas");
        Ok(())
    }

    /// Both taddrs report the manifest's transparent balances on both ingest paths. Whole
    /// replies are compared, so a divergent `received` fails alongside a divergent `balance`.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance_fetch_vs_state() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        assert!(
            cache.manifest.faucet.balances.transparent > 0,
            "the fixture's faucet holds transparent coinbase"
        );
        for (taddr, balance) in [
            (
                &cache.manifest.faucet.transparent,
                cache.manifest.faucet.balances.transparent,
            ),
            (
                &cache.manifest.recipient.transparent,
                cache.manifest.recipient.balances.transparent,
            ),
        ] {
            let params = json!([{ "addresses": [taddr] }]);
            let fetch_bal = fetch
                .json_rpc()
                .call_value("getaddressbalance", params.clone())
                .await?;
            let state_bal = state
                .json_rpc()
                .call_value("getaddressbalance", params)
                .await?;
            assert_eq!(
                fetch_bal.get("balance").and_then(Value::as_u64),
                Some(balance),
                "getaddressbalance must report the manifest balance of {taddr}: {fetch_bal}"
            );
            assert_eq!(fetch_bal, state_bal);
        }
        Ok(())
    }

    /// Draining the 100 blocks below the tip plus six past it yields the 100
    /// available blocks (fetch == state) and then errors rather than ending cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_upper_bound() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let tip = cache.manifest.fixture.height;
        let (start, end) = (tip - 99, tip + 6);
        let (fetch_blocks, fetch_errored) =
            fetch.drain_block_range(start, end, all_pools.clone()).await;
        let (state_blocks, state_errored) =
            state.drain_block_range(start, end, all_pools.clone()).await;

        assert_eq!(fetch_blocks, state_blocks);
        let compact_block = state_blocks.last().expect("non-empty range");
        assert_eq!(
            compact_block.height,
            u64::from(tip),
            "the drain must stop at the tip, not at the requested end"
        );
        assert_eq!(fetch_blocks.len(), 100);
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }

    /// Draining the inverted range [tip + 6, 1] yields no blocks (fetch == state,
    /// both empty) and then errors rather than ending cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_lower_bound() -> Result<()> {
        let cache = ChainCache::ensure().await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
        let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state indexer must answer and the fetch one must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = zebrad.best_block_hash().await?;
        let deltas = json!([&tip_hash]);
        state
            .json_rpc()
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this indexer pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = (cache.manifest.fixture.height + 6, 1u32);
        let (fetch_blocks, fetch_errored) =
            fetch.drain_block_range(start, end, all_pools.clone()).await;
        let (state_blocks, state_errored) =
            state.drain_block_range(start, end, all_pools.clone()).await;

        assert_eq!(fetch_blocks, state_blocks);
        assert!(
            fetch_blocks.is_empty(),
            "a descending range starting past the tip serves nothing: {fetch_blocks:?}"
        );
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }
}
