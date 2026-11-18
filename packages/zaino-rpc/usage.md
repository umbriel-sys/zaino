# `zaino-rpc` — usage

The JSON-RPC transport. HTTP, the request/response envelope, authentication,
and one retry rule. Nothing else.

## Scope, stated precisely

`call()` returns a raw `serde_json::Value`. **Response parsing is the adapter's
job**, not this crate's:

```rust
use zaino_rpc::{RpcClient, RpcClientConfig};

let client = RpcClient::new(config)?;
let value = client.call("getblockchaininfo", vec![]).await?;
// -> serde_json::Value; interpreting it is zaino-source-zcash-rpc's problem
```

This is the boundary that lets one client serve both the production adapter
(`zaino-source-zcash-rpc`, which parses into domain types) and the live tests'
`ValidatorOracle` (which compares raw JSON against Zaino's answers). An oracle
that shared the parser under test would not be an oracle.

It is also why replacing `zaino-fetch` took two crates and not one:
`zaino-fetch` did transport *and* parsing *and* server-side serialization in one
place, and each of the three has a different owner now.

## Errors

`RpcError` distinguishes the envelope's failure modes, and converts into
`zaino_source::NonDomainError` so the source layer's `FailureMode` classification
works end to end:

```rust
RpcError::Rpc { code, message }   // the server answered with an error object
RpcError::Http(status)            // transport-level status
RpcError::Transport(..)           // connection, timeout
RpcError::Auth                    // credentials rejected
```

The `code` on `RpcError::Rpc` is the thing a legacy-compatible client keys on,
so it must survive to the served response. It does, via
`FailureMode::RpcError(i64)` and the downcast walk in `zaino-serve`.

## The one retry this crate does

`-1` (work queue full) is retried here, at the transport, because it is a
property of the connection rather than of the question. Everything else is left
to `zaino_source::ValidatorClient`, which has the policy and the backoff. Do not add
retry rules here — the layer above cannot see or override them.

## Probing a validator at startup

```rust
use zaino_rpc::{auth_from_parts, probe_node};

let auth = auth_from_parts(user, password)?;
let info = probe_node(&addr, auth).await?;   // 6 attempts, 3s apart
```

`probe_node` **returns an error**. Its predecessor in `zaino-fetch` called
`std::process::exit(1)`, which made the startup path untestable and gave an
embedding process no say in its own shutdown.

## Address resolution and the crypto provider

Two things every network endpoint in Zaino needs live here, so no crate has
to pull in a configuration crate to get them:

- `net::resolve_socket_addr` / `net::try_resolve_address`: `host:port` to a
  `SocketAddr`. `try_resolve_address` answers an `AddressResolution`, which
  separates a well-formed hostname DNS cannot resolve yet (Docker-style
  deferred resolution, accepted) from a malformed address (always an error).
- `ensure_default_crypto_provider`: installs rustls's `aws-lc-rs` provider as
  the process default if none is installed. `RpcClient::new` calls it; a TLS
  server must call it before building its acceptor. First-install-wins, so an
  embedder that installed its own provider keeps it (ADR-0006).

## Metrics

| Metric                                           | Meaning                                                   |
| ------------------------------------------------ | --------------------------------------------------------- |
| `zaino.rpc.outbound.duration_seconds{method}`    | one attempt that got a response (retry sleeps excluded)   |
| `zaino.rpc.outbound.errors_total{method,outcome}`| `transport_error` unreachable / `rpc_error` refused / `retried` saturated |

- Attempt volume = `duration_seconds` `_count` (no success counter)
- Rising `retried` share = validator queue filling (a refusal is fast → latency drops, share rises)
- `method` = a label → `call` takes `&'static str` (cardinality capped at compile time; the
  recorder never evicts)

## Related

- `zaino-source-zcash-rpc` — parses what this returns.
- ADR-0008 — where this sits in the crate graph.
