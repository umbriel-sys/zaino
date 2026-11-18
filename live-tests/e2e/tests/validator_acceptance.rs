//! Guarantees exported from zingolib's regtest suite under its regtest
//! removal (zingolib `docs/testing/regtest-port-plan.md`, ruling 4): each one
//! is about the validator or the indexer rather than the wallet, so it lives
//! here, on the harness that launches both.
//!
//! - the indexer's mempool view trails the validator's acceptance within a bound;
//! - the indexer converges with the validator after a generated block;
//! - the validator accepts a spend of a tip-block note, an aged note, a young
//!   coinbase and an aged coinbase, to Orchard and to Sapling receivers.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use zaino_testutils::{
    Backend, ChainCache, Party, Validator as _, Wallet as _, Zainod, Zebrad, FILLER_ADDRESS,
};

use e2e::Pool;

/// Indexer sync timeout.
const READY: Duration = Duration::from_secs(300);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// The spend the acceptance cells make back toward the faucet.
const SPEND_AMOUNT: u64 = 20_000;
/// zingolib's bound on the indexer's mempool ingestion after validator acceptance.
const MEMPOOL_INGEST_BOUND: Duration = Duration::from_secs(10);
/// The indexer mirrors the validator's mempool on a 500 ms cadence.
const MEMPOOL_POLL: Duration = Duration::from_millis(50);
const AGED_NOTE_BLOCKS: u32 = 3;
const AGED_COINBASE_BLOCKS: u32 = 10;

/// Whether the validator's mempool holds `txid`.
async fn validator_mempool_holds(zebrad: &Zebrad, txid: &str) -> Result<bool> {
    let mempool = zebrad.rpc().call_value("getrawmempool", json!([])).await?;
    Ok(mempool
        .as_array()
        .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(txid))))
}

/// H-LAG and H-FAITHFUL-ACCEPT: after the validator accepts a transaction, the
/// indexer's mempool shows it within the bound.
#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_mempool_view_trails_validator_acceptance(#[case] backend: Backend) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    indexer
        .wait_for_block(cache.manifest.fixture.height, READY)
        .await?;

    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;
    let ua = recipient.address(Pool::Orchard).await?;
    let before = recipient.balances().await?;
    let txid = faucet.send(&ua, SEND_AMOUNT).await?.to_string();
    let accepted_at = Instant::now();

    anyhow::ensure!(
        validator_mempool_holds(&zebrad, &txid).await?,
        "the validator's own mempool must hold {txid} as soon as the send returns"
    );

    let irpc = indexer.json_rpc();
    let lag = loop {
        let indexer_mempool = irpc.call_value("getrawmempool", json!([])).await?;
        if indexer_mempool
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(&txid)))
        {
            break accepted_at.elapsed();
        }
        anyhow::ensure!(
            accepted_at.elapsed() < MEMPOOL_INGEST_BOUND,
            "the validator accepted {txid} {MEMPOOL_INGEST_BOUND:?} ago and the indexer's \
             mempool still does not show it"
        );
        tokio::time::sleep(MEMPOOL_POLL).await;
    };
    println!("indexer mempool ingestion lag after validator acceptance: {lag:?}");
    anyhow::ensure!(
        lag < MEMPOOL_INGEST_BOUND,
        "indexer mempool lag {lag:?} exceeded the {MEMPOOL_INGEST_BOUND:?} bound"
    );

    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;
    recipient.sync().await?;
    let after = recipient.balances().await?;
    assert_eq!(
        after.get(Pool::Ironwood) - before.get(Pool::Ironwood),
        SEND_AMOUNT,
        "the confirmed send must land in the recipient's ironwood pool: {before:?} -> {after:?}"
    );
    Ok(())
}

/// The indexer serves the fixture tip from its cached database, and the
/// validator's new tip after a generated block.
#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_converges_with_validator_after_block_generation(
    #[case] backend: Backend,
) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;

    let irpc = indexer.json_rpc();
    let served_height = || async {
        irpc.call_value("getblockcount", json!([]))
            .await?
            .as_u64()
            .context("getblockcount returns a number")
    };

    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;
    let indexer_height = served_height().await?;
    anyhow::ensure!(
        indexer_height >= u64::from(tip),
        "at launch: indexer served tip {indexer_height} behind the fixture tip {tip}"
    );

    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;
    let ua = recipient.address(Pool::Orchard).await?;
    let txid = faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;
    let indexer_height = served_height().await?;
    anyhow::ensure!(
        indexer_height >= u64::from(tip),
        "after the block: indexer served tip {indexer_height} behind validator tip {tip}"
    );
    let mined = indexer.get_block(tip).await?;
    anyhow::ensure!(
        mined.vtx.iter().any(|tx| tx.txid == txid.as_ref().to_vec()),
        "the served block {tip} must carry the confirmed send {txid}"
    );
    Ok(())
}

/// The validator accepts a recipient's spend of a note received at the tip, and
/// of a note aged a few blocks, to each shielded receiver.
#[rstest]
#[case::tip_note_to_orchard(Pool::Orchard, 0)]
#[case::tip_note_to_sapling(Pool::Sapling, 0)]
#[case::aged_note_to_orchard(Pool::Orchard, AGED_NOTE_BLOCKS)]
#[case::aged_note_to_sapling(Pool::Sapling, AGED_NOTE_BLOCKS)]
#[tokio::test(flavor = "multi_thread")]
async fn received_note_spends_to_receiver(#[case] target: Pool, #[case] age: u32) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    indexer
        .wait_for_block(cache.manifest.fixture.height, READY)
        .await?;

    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

    if age == 0 {
        let ua = recipient.address(Pool::Orchard).await?;
        let before = recipient.balances().await?;
        faucet.send(&ua, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;
        let after = recipient.balances().await?;
        assert_eq!(
            after.get(Pool::Ironwood) - before.get(Pool::Ironwood),
            SEND_AMOUNT,
            "the tip-block note must be spendable: {before:?} -> {after:?}"
        );
    } else {
        let newest = cache
            .manifest
            .events
            .iter()
            .filter(|event| event.to == Party::Recipient)
            .map(|event| event.height)
            .max()
            .context("the fixture records a receipt by the recipient")?;
        let depth = cache.manifest.fixture.height - newest;
        anyhow::ensure!(
            depth >= age,
            "the recipient's newest note is {depth} blocks deep, not yet aged {age}"
        );
    }

    let target_addr = faucet.address(target).await?;
    let txid = recipient
        .send(&target_addr, SPEND_AMOUNT)
        .await
        .with_context(|| {
            format!("the validator must accept a spend of a note aged {age} to {target:?}")
        })?;
    anyhow::ensure!(
        validator_mempool_holds(&zebrad, &txid.to_string()).await?,
        "the validator's mempool must hold the accepted spend {txid}"
    );
    Ok(())
}

/// The validator accepts the miner's spend of its coinbase at the tip, and of a
/// coinbase aged ten blocks, to each shielded receiver.
#[rstest]
#[case::young_coinbase_to_ironwood(Pool::Orchard, 0)]
#[case::young_coinbase_to_sapling(Pool::Sapling, 0)]
#[case::aged_coinbase_to_orchard(Pool::Orchard, AGED_COINBASE_BLOCKS)]
#[tokio::test(flavor = "multi_thread")]
async fn coinbase_spends_to_receiver(#[case] target: Pool, #[case] age: u32) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

    // The faucet's shielded coinbase band sits below the first recorded
    // transaction: the newest block whose coinbase carries ironwood actions.
    let first_event = cache
        .manifest
        .events
        .iter()
        .map(|event| event.height)
        .min()
        .context("the fixture records transactions")?;
    let newest_coinbase = indexer
        .get_block_range(2, first_event - 1)
        .await?
        .iter()
        .filter(|block| {
            block
                .vtx
                .iter()
                .any(|tx| tx.index == 0 && !tx.ironwood_actions.is_empty())
        })
        .map(|block| block.height)
        .max()
        .map(u32::try_from)
        .transpose()?
        .context("the faucet holds shielded coinbase below the first recorded transaction")?;
    let depth = tip - newest_coinbase;
    if age == 0 {
        // A young coinbase is the one thing the fixture cannot hold: mine one
        // block to the faucet's miner address, so its newest note is at the tip.
        let tip = zebrad.generate_blocks(1).await?;
        indexer.wait_for_block(tip, READY).await?;
        faucet.sync().await?;
    } else {
        anyhow::ensure!(
            depth >= age,
            "the faucet's newest coinbase is {depth} blocks deep, not yet aged {age}"
        );
    }

    // More than every non-coinbase ironwood note the faucet holds, so the
    // spend must draw on a coinbase note.
    let received: u64 = cache
        .manifest
        .events
        .iter()
        .filter(|event| event.to == Party::Faucet)
        .map(|event| event.amount)
        .sum();
    let spend = received + SPEND_AMOUNT;
    let target_addr = recipient.address(target).await?;
    let txid = faucet.send(&target_addr, spend).await.with_context(|| {
        format!(
            "the validator must accept the miner's spend of a coinbase aged {age} to {target:?}"
        )
    })?;
    anyhow::ensure!(
        validator_mempool_holds(&zebrad, &txid.to_string()).await?,
        "the validator's mempool must hold the accepted coinbase spend {txid}"
    );
    Ok(())
}
