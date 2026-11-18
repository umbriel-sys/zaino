//! Address-index requests zebrad 7 refuses are refused identically by zainod on
//! both backends: same JSON-RPC code (`-5` TEX, `-8` too many addresses) and
//! message, whichever path answers.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use zaino_testutils::{
    indexer::rpc_error, Backend, ChainCache, Validator as _, Zainod, Zebrad, FILLER_ADDRESS,
};

const READY: Duration = Duration::from_secs(180);
const TEX: &str = "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy";
/// One past zebrad 7's per-request address limit.
const TOO_MANY: usize = 10_001;

#[tokio::test(flavor = "multi_thread")]
async fn address_queries_refuse_what_zebrad_refuses() -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let fetch = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
    let state = Zainod::launch(cache.indexer_config(&zebrad, Backend::State)).await?;

    let tip = cache.manifest.fixture.height;
    for indexer in [&fetch, &state] {
        indexer.wait_for_block(tip, READY).await?;
    }

    let too_many = vec![FILLER_ADDRESS; TOO_MANY];
    let requests = [
        ("getaddressbalance", json!([{ "addresses": [TEX] }]), -5),
        ("getaddressutxos", json!([{ "addresses": [TEX] }]), -5),
        (
            "getaddresstxids",
            json!([{ "addresses": [TEX], "start": 1, "end": tip }]),
            -5,
        ),
        ("getaddressbalance", json!([{ "addresses": too_many }]), -8),
        ("getaddressutxos", json!([{ "addresses": too_many }]), -8),
        (
            "getaddresstxids",
            json!([{ "addresses": too_many, "start": 1, "end": tip }]),
            -8,
        ),
    ];

    let vrpc = zebrad.rpc();
    for (method, params, code) in requests {
        let expected = rpc_error(vrpc, method, params.clone()).await?;
        assert_eq!(expected["code"], code, "zebrad {method}: {expected}");
        for indexer in [&fetch, &state] {
            let served = rpc_error(&indexer.json_rpc(), method, params.clone()).await?;
            assert_eq!(served, expected, "{method}");
        }
    }
    Ok(())
}
