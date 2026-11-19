//! `getblock` is zebrad's answer at every verbosity, by hash and by height,
//! for every block that holds a recorded transaction and at genesis and tip;
//! an invalid verbosity is refused as zebrad refuses it.

use std::time::Duration;

use anyhow::Result;
use rstest::rstest;
use serde_json::json;
use zaino_testutils::{
    indexer::rpc_error, rpc::assert_rpc_parity, Backend, ChainCache, Validator as _, Zainod,
    Zebrad,
};

const READY: Duration = Duration::from_secs(180);

#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn getblock_is_zebrads_answer(#[case] backend: Backend) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let mut heights: Vec<u32> = cache.manifest.events.iter().map(|event| event.height).collect();
    heights.extend([0, 1, tip]);
    heights.sort_unstable();
    heights.dedup();

    let served = indexer.json_rpc();
    let vrpc = zebrad.rpc();
    for height in heights {
        for verbosity in 0..=3 {
            let by_height = assert_rpc_parity(
                "getblock",
                &format!("[\"{height}\", {verbosity}]"),
                vrpc,
                &served,
                &[],
            )
            .await?;
            if verbosity == 1 {
                let hash = by_height["hash"]
                    .as_str()
                    .expect("a verbose block names its hash")
                    .to_owned();
                assert_rpc_parity(
                    "getblock",
                    &format!("[\"{hash}\", 1]"),
                    vrpc,
                    &served,
                    &[],
                )
                .await?;
            }
        }
    }

    let params = json!([tip.to_string(), 4]);
    let expected = rpc_error(vrpc, "getblock", params.clone()).await?;
    assert_eq!(rpc_error(&served, "getblock", params).await?, expected);
    Ok(())
}
