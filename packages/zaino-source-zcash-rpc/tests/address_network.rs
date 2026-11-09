//! A node that answers another network's address empty is held to the
//! contract by the adapter: `InvalidAddress`, before the node is asked.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{GetAddressUtxosError, OneShotGetAddressUtxos, QueryError};
use zaino_source_zcash_rpc::{Quirks, ZcashRpcAdapter};

const MAINNET: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
const TESTNET: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

/// A regtest node answering every address query empty, logging each method.
async fn regtest_node(listener: TcpListener, calls: Arc<Mutex<Vec<String>>>) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let calls = Arc::clone(&calls);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let Ok(n) = stream.read(&mut buf).await else {
                return;
            };
            let text = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());
            let body = text.split("\r\n\r\n").nth(1).unwrap_or_default();
            let request: serde_json::Value = serde_json::from_str(body).expect("JSON-RPC request");
            let method = request["method"].as_str().unwrap_or_default().to_owned();
            let result = match method.as_str() {
                "getblockchaininfo" => serde_json::json!({ "chain": "regtest" }),
                _ => serde_json::json!([]),
            };
            calls.lock().expect("call log").push(method);
            let reply = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result, "error": null })
                .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("reply written");
        });
    }
}

async fn adapter(quirks: Quirks) -> (ZcashRpcAdapter, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let calls = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(regtest_node(listener, Arc::clone(&calls)));
    let rpc = RpcClient::new(RpcClientConfig {
        url: format!("http://127.0.0.1:{port}"),
        auth: None,
        ..Default::default()
    })
    .expect("RPC client");
    (ZcashRpcAdapter::with_quirks(rpc, quirks), calls)
}

fn unchecked() -> Quirks {
    Quirks {
        unchecked_address_network: true,
        ..Quirks::default()
    }
}

/// Under the quirk a mainnet address on regtest is refused without asking the
/// node; the network is read once.
#[tokio::test]
async fn another_networks_address_is_invalid_under_the_quirk() {
    let (adapter, calls) = adapter(unchecked()).await;
    for _ in 0..2 {
        let answer = adapter
            .get_address_utxos(vec![TESTNET.into(), MAINNET.into()])
            .await;
        assert!(
            matches!(
                answer,
                Err(QueryError::Domain(GetAddressUtxosError::InvalidAddress(_)))
            ),
            "{answer:?}"
        );
    }
    assert_eq!(*calls.lock().expect("call log"), ["getblockchaininfo"]);
}

/// The node's own network passes through to the node.
#[tokio::test]
async fn the_nodes_network_is_asked() {
    let (adapter, calls) = adapter(unchecked()).await;
    let answer = adapter.get_address_utxos(vec![TESTNET.into()]).await;
    assert!(
        matches!(answer, Ok(ref utxos) if utxos.is_empty()),
        "{answer:?}"
    );
    assert_eq!(
        *calls.lock().expect("call log"),
        ["getblockchaininfo", "getaddressutxos"]
    );
}

/// Without the quirk the node checks, so the adapter asks nothing else.
#[tokio::test]
async fn without_the_quirk_the_node_checks() {
    let (adapter, calls) = adapter(Quirks::default()).await;
    let answer = adapter.get_address_utxos(vec![MAINNET.into()]).await;
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(*calls.lock().expect("call log"), ["getaddressutxos"]);
}
