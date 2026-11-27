//! The compact block over JSON-RPC: `getblock` verbosity 0 for the block and
//! verbosity 1 for its tree sizes, asked together and paired by hash.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use zaino_primitives::types::{Height, PoolFilter, TreeSize};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{FailureMode, GetBlockError, OneShotGetCompactBlock, QueryError};
use zaino_source_zcash_rpc::ZcashRpcAdapter;

const HEIGHT: u32 = 1_000_000;
const HASH: &str = "000000000062eff9ae053020017bfef24e521a2704c5ec9ead2a4608ac70fc7a";
const OTHER: &str = "0000000001512dea98f49c5890d45f03b755bfea6adb4b284aa3eb3aa46af377";

/// The verbose block the validator answers at [`HEIGHT`], naming `hash`.
fn verbose(hash: &str) -> serde_json::Value {
    serde_json::json!({
        "hash": hash,
        "confirmations": 10,
        "height": HEIGHT,
        "version": 4,
        "merkleroot": "11".repeat(32),
        "tx": ["22".repeat(32)],
        "time": 1_602_206_541u32,
        "nonce": "33".repeat(32),
        "solution": "44",
        "bits": "1c01b3cd",
        "difficulty": 1.0,
        "trees": { "sapling": { "size": 7 }, "orchard": { "size": 9 } },
        "previousblockhash": OTHER,
    })
}

/// Answers `getblock` at [`HEIGHT`]: the fixture's bytes at verbosity 0, the
/// verbose block naming `verbose_hash` at verbosity 1, `-8` anywhere else.
/// Records each request's parameters.
async fn validator(listener: TcpListener, verbose_hash: &'static str, calls: Calls) {
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/block_{HEIGHT}.hex",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the fixture")
    .trim()
    .to_owned();
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let (raw, calls) = (raw.clone(), Arc::clone(&calls));
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            let read = stream.read(&mut buffer).await.expect("a request");
            let text = String::from_utf8_lossy(&buffer[..read]);
            let body = text.split("\r\n\r\n").nth(1).expect("a body");
            let request: serde_json::Value = serde_json::from_str(body).expect("JSON-RPC");
            let params = request["params"].clone();
            calls.lock().expect("calls").push(params.to_string());
            let at_height = params[0].as_str() == Some(&HEIGHT.to_string());
            let answer = match (at_height, params[1].as_u64()) {
                (true, Some(0)) => serde_json::json!({ "result": raw }),
                (true, Some(1)) => serde_json::json!({ "result": verbose(verbose_hash) }),
                _ => serde_json::json!({
                    "result": null,
                    "error": { "code": -8, "message": "Block not found" }
                }),
            };
            let mut reply = answer;
            reply["jsonrpc"] = "2.0".into();
            reply["id"] = request["id"].clone();
            let reply = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len(),
            );
            stream.write_all(response.as_bytes()).await.expect("sent");
        });
    }
}

type Calls = Arc<Mutex<Vec<String>>>;

async fn adapter(verbose_hash: &'static str) -> (ZcashRpcAdapter, Calls) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let port = listener.local_addr().expect("an address").port();
    let calls = Calls::default();
    tokio::spawn(validator(listener, verbose_hash, Arc::clone(&calls)));
    let rpc = RpcClient::new(RpcClientConfig {
        url: format!("http://127.0.0.1:{port}"),
        ..RpcClientConfig::default()
    })
    .expect("builds");
    (ZcashRpcAdapter::new(rpc), calls)
}

/// The block's compact data and the verbose block's tree sizes come back as
/// one compact block, from exactly two calls.
#[tokio::test]
async fn a_compact_block_is_the_block_with_its_tree_sizes() {
    let (adapter, calls) = adapter(HASH).await;
    let height = Height::try_from(HEIGHT).expect("a height");
    let compact = adapter
        .get_compact_block(height, PoolFilter::all())
        .await
        .expect("answered");

    assert_eq!(compact.hash.to_string(), HASH);
    assert_eq!(compact.prev_hash.to_string(), OTHER);
    assert_eq!(compact.height, HEIGHT);
    assert_eq!(compact.transactions.len(), 6);
    assert_eq!(compact.chain_metadata.sapling_tree_size, TreeSize::from(7));
    assert_eq!(compact.chain_metadata.orchard_tree_size, TreeSize::from(9));
    assert_eq!(compact.chain_metadata.ironwood_tree_size, TreeSize::ZERO);

    let mut asked = calls.lock().expect("calls").clone();
    asked.sort();
    assert_eq!(
        asked,
        vec![format!("[\"{HEIGHT}\",0]"), format!("[\"{HEIGHT}\",1]")]
    );
}

/// The filter is applied by the adapter: a pool it leaves out is cleared and
/// every transaction is kept.
#[tokio::test]
async fn the_filter_is_applied_and_every_transaction_kept() {
    let (adapter, _) = adapter(HASH).await;
    let height = Height::try_from(HEIGHT).expect("a height");
    let whole = adapter
        .get_compact_block(height, PoolFilter::all())
        .await
        .expect("answered");
    assert!(whole
        .transactions
        .iter()
        .any(|tx| !tx.transparent_outputs.is_empty()));

    let shielded = adapter
        .get_compact_block(height, PoolFilter::default())
        .await
        .expect("answered");
    assert_eq!(shielded.transactions.len(), whole.transactions.len());
    assert!(shielded
        .transactions
        .iter()
        .all(|tx| tx.transparent_inputs.is_empty() && tx.transparent_outputs.is_empty()));
    assert_eq!(shielded.chain_metadata, whole.chain_metadata);
}

/// Two replies naming different blocks are not paired: the read fails as
/// invalid validator data, naming both.
#[tokio::test]
async fn replies_naming_different_blocks_are_refused() {
    let (adapter, _) = adapter(OTHER).await;
    let height = Height::try_from(HEIGHT).expect("a height");
    let error = adapter
        .get_compact_block(height, PoolFilter::all())
        .await
        .expect_err("refused");
    let QueryError::NonDomain(error) = error else {
        panic!("a domain answer: {error:?}");
    };
    assert_eq!(error.mode, FailureMode::InvalidSourceData);
    let text = error.to_string();
    assert!(text.contains(HASH) && text.contains(OTHER), "{text}");
}

/// A height the validator has no block at is the port's own refusal.
#[tokio::test]
async fn a_missing_height_is_height_not_found() {
    let (adapter, _) = adapter(HASH).await;
    let height = Height::try_from(HEIGHT + 1).expect("a height");
    let error = adapter
        .get_compact_block(height, PoolFilter::all())
        .await
        .expect_err("refused");
    assert!(
        matches!(error, QueryError::Domain(GetBlockError::HeightNotFound(h)) if h == height),
        "{error:?}"
    );
}
