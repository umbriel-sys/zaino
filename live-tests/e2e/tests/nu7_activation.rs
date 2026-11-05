//! Wallet-tier predicates across the NU7 activation boundary.
//!
//! Every test runs on the `nu7` boundary fixture: Ironwood from height 2 and
//! NU7 from [`NU7_BOUNDARY`], pinned once in [`nu7_schedule`] and adopted by
//! validator, indexer and wallet alike, with the tip two blocks below the
//! boundary and the faucet holding three Ironwood coinbase notes. NU7 changes
//! no transaction format: what a wallet must get right is the consensus branch
//! its signatures commit to, which flips at the boundary. So the cells here are
//! the ones only a wallet on both sides can cover: a transaction signed for NU7
//! is accepted by the validator and served by zaino, and receipts confirm in
//! the last NU6.3 block and in the activation block alike.

use std::time::Duration;

use anyhow::Result;
use zaino_testutils::{
    ActivationHeights, Backend, ChainCache, Fixture, Party, Pool, Validator as _, Wallet as _,
    Zainod, Zebrad, FILLER_ADDRESS, REGTEST_ACTIVATION_HEIGHTS,
};

use e2e::assert_pool_present;

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
/// NU6.3 (Ironwood) from height 2, NU7 from here.
const NU7_BOUNDARY: u32 = 6;

fn nu7_schedule() -> ActivationHeights {
    ActivationHeights {
        nu7: Some(NU7_BOUNDARY),
        ..REGTEST_ACTIVATION_HEIGHTS
    }
}

/// After activation, a send built and signed for the NU7 branch is mined by the
/// validator, served by zaino as Ironwood compact data, and lands in the
/// recipient's Ironwood pool.
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_send_confirms_after_nu7_activation() -> Result<()> {
    let cache =
        ChainCache::ensure_fixture(Fixture::boundary("nu7", nu7_schedule(), NU7_BOUNDARY)).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

    // Past the boundary before anything is built: the send targets an NU7 block.
    let tip = zebrad
        .generate_blocks_to(NU7_BOUNDARY - cache.manifest.fixture.height, FILLER_ADDRESS)
        .await?;
    assert_eq!(tip, NU7_BOUNDARY);
    indexer.wait_for_block(tip, READY).await?;
    faucet.sync().await?;
    assert!(
        faucet.balances().await?.get(Pool::Ironwood) > 0,
        "the faucet's coinbase notes are ironwood"
    );

    let before = recipient.balances().await?;
    let ua = recipient.address(Pool::Orchard).await?;
    let txid = faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    indexer.wait_for_block(tip, READY).await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    assert_eq!(
        balance.get(Pool::Ironwood) - before.get(Pool::Ironwood),
        SEND_AMOUNT
    );
    assert_eq!(balance.get(Pool::Orchard), 0);

    let mined = indexer.get_block(tip).await?;
    assert!(u32::try_from(mined.height)? > NU7_BOUNDARY);
    assert_pool_present(&mined, &txid, Pool::Ironwood);
    Ok(())
}

/// Receipts confirm on both sides of the boundary: a send confirmed in the last
/// NU6.3 block (boundary − 1), then a send built at that tip but confirmed in the
/// activation block itself, so its signature commits to the NU7 branch. The
/// pre-boundary receipt survives the flip unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn receipts_confirm_on_both_sides_of_the_nu7_boundary() -> Result<()> {
    let cache =
        ChainCache::ensure_fixture(Fixture::boundary("nu7", nu7_schedule(), NU7_BOUNDARY)).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
    let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

    // The fixture's tip is boundary − 2, so the first send confirms at boundary − 1.
    let before = recipient.balances().await?;
    let ua = recipient.address(Pool::Orchard).await?;
    let last_nu6_3_txid = faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    assert_eq!(tip, NU7_BOUNDARY - 1);
    indexer.wait_for_block(tip, READY).await?;
    recipient.sync().await?;
    assert_eq!(
        recipient.balances().await?.get(Pool::Ironwood) - before.get(Pool::Ironwood),
        SEND_AMOUNT,
        "receipt confirmed at boundary - 1"
    );

    // Built at boundary − 1, confirmed in the activation block: an NU7 signature.
    faucet.sync().await?;
    let first_nu7_txid = faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    assert_eq!(tip, NU7_BOUNDARY);
    indexer.wait_for_block(tip, READY).await?;
    recipient.sync().await?;
    assert_eq!(
        recipient.balances().await?.get(Pool::Ironwood) - before.get(Pool::Ironwood),
        2 * SEND_AMOUNT,
        "the activation-block receipt lands beside the pre-boundary one"
    );

    let blocks = indexer.get_block_range(1, tip).await?;
    let last_nu6_3_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU7_BOUNDARY - 1))
        .expect("boundary-1 block served");
    let activation_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU7_BOUNDARY))
        .expect("boundary block served");
    assert_pool_present(last_nu6_3_block, &last_nu6_3_txid, Pool::Ironwood);
    assert_pool_present(activation_block, &first_nu7_txid, Pool::Ironwood);
    Ok(())
}
