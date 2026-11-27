//! JSON-RPC client and the parity helper shared by the parity tests.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::json::json_equal_ignoring;

/// A JSON-RPC 2.0 client over plain HTTP, no auth.
#[derive(Clone, Debug)]
pub struct JsonRpcClient {
    url: String,
    http: reqwest::Client,
}

impl JsonRpcClient {
    /// A client for `http://<addr>`.
    pub fn new(addr: SocketAddr) -> Self {
        // reqwest links rustls without a provider; the client refuses to build without one.
        zaino_rpc::ensure_default_crypto_provider();
        Self {
            url: format!("http://{addr}"),
            http: reqwest::Client::new(),
        }
    }

    /// The server URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Calls `method` with `params` and returns `result`. An `error` reply
    /// is `Err` whose message is `RPC error: <error json>`.
    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let reply: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method}: request to {}", self.url))?
            .json()
            .await
            .with_context(|| format!("{method}: reply is not JSON"))?;
        if let Some(error) = reply.get("error").filter(|e| !e.is_null()) {
            bail!("{method}: RPC error: {error}");
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Call `method(params)` on two JSON-RPC sources and assert the responses
/// agree, after dropping `volatile` (dot-separated) paths. Returns the
/// first source's response so the caller can pluck fields from it for
/// follow-up assertions. `params` is a JSON array, or empty for `[]`.
pub async fn assert_rpc_parity(
    method: &'static str,
    params: &str,
    a: &JsonRpcClient,
    b: &JsonRpcClient,
    volatile: &[&str],
) -> Result<Value> {
    let params: Value = if params.is_empty() {
        Value::Array(Vec::new())
    } else {
        serde_json::from_str(params)?
    };
    let av = a.call_value(method, params.clone()).await?;
    let bv = b.call_value(method, params).await?;
    json_equal_ignoring(method, av.clone(), bv, volatile)?;
    Ok(av)
}
