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

This is the boundary that keeps parsing out of the transport: the production
adapter (`zaino-source-zcash-rpc`) parses into domain types on top of it, and
a caller that wants the raw JSON gets it untouched. The live tests compare
raw JSON from the validator against Zaino's answers through a client of
their own, because an oracle that shared the parser under test would not be
an oracle.

Transport, parsing and server-side serialization each have a different owner:
this crate, `zaino-source-zcash-rpc` and `zainod`.

## Errors

`RpcError` distinguishes the envelope's failure modes, and converts into
`zaino_source::NonDomainError` so the source layer's `FailureMode` classification
works end to end:

```rust
RpcError::Rpc { code, message }   // the validator answered with an error object
RpcError::Http(..)                // connection, timeout
RpcError::Status(status)          // a failing HTTP status with no error object
RpcError::Busy { attempts, status } // 429 or 503 through every retry
RpcError::Saturated { .. }        // no request slot came free in this client
```

The `code` on `RpcError::Rpc` is the thing a legacy-compatible client keys on,
so it must survive to the served response. It does, via
`FailureMode::RpcError(i64)` and the error mapping in `zainod::serve`. An
error object is read whatever HTTP status carried it: zebrad and zakura answer
a request they cannot read with a 400 or 413 and the error in the body. Two
kinds of status are the answer whatever their body says: 429 and 503 (busy),
and 401 and 403 (`FailureMode::Auth`; zebrad's 401 carries an error object
whose code is the status).

A connection error does not carry the validator's URL: the text reaches
Zaino's own clients.

## Requests in flight

A client keeps at most `RpcClientConfig::max_in_flight` requests in flight
(default `DEFAULT_MAX_IN_FLIGHT`, 64). zebrad serves 100 requests at once and
answers every other with HTTP 429, so the bound keeps one client's whole load
under that with room for the validator's other clients. Share one `RpcClient`
between everything that talks to one validator: two clients are two budgets.

A call past the bound waits its turn, in order, for no longer than its request
timeout. If no slot comes it fails with `RpcError::Saturated`, which the source
layer classifies as `FailureMode::Timeout`. A slot is held for one request
only, never across a retry sleep.

## The retry this crate does

A call the validator was too busy to take is retried here, at the transport,
because it is a property of the connection rather than of the question. Busy
is an HTTP status: 429 (zebrad and zakura past 100 requests in flight, so
another client has filled the validator) or 503. Up to `max_retries` times
(default 5), `retry_delay` apart (default 500 ms); after that the call fails
with `RpcError::Busy`, which the source layer classifies as
`FailureMode::Busy`.

No JSON-RPC error code is a busy signal, and an error object is never asked
again. In particular `-1` is not "work queue full": zebrad and zakura answer
it for any failed request, invalid parameters among them.

Everything else is left to `zaino_source::ValidatorClient`, which has the
policy and the backoff. Do not add retry rules here — the layer above cannot
see or override them.

## Probing a validator at startup

```rust
use zaino_rpc::{auth_from_parts, probe_node};

let auth = auth_from_parts(user, password)?;
let info = probe_node(&addr, auth).await?;   // 6 attempts, 3s apart
```

`auth_from_parts(cookie_path, user, password)` sends nothing when nothing is
configured (`Ok(None)`), a cookie where a path is given, the pair where both
are given, and refuses a user without a password or the reverse
(`IncompleteCredentials`). There is no stand-in pair.

`probe_node` **returns an error**. It never exits the process: an exit would
make the startup path untestable and give an embedding process no say in its
own shutdown.

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
  embedder that installed its own provider keeps it.

## Metrics

| Metric                                           | Meaning                                                   |
| ------------------------------------------------ | --------------------------------------------------------- |
| `zaino.rpc.outbound.duration_seconds{method}`    | one attempt that got a response (retry sleeps excluded)   |
| `zaino.rpc.outbound.errors_total{method,outcome}`| `transport_error` unreachable / `rpc_error` refused / `retried` validator busy, asked again / `busy` still busy after every retry / `saturated` no slot came free |

- Attempt volume = `duration_seconds` `_count` (no success counter)
- Rising `retried` share = the validator is busy (HTTP 429 or 503): something else is filling
  it, since this client stays under its limit. A refusal is fast → latency drops, share rises
- Any `saturated` = this client's `max_in_flight` slots stayed full for a whole request timeout
- `method` = a label → `call` takes `&'static str` (cardinality capped at compile time; the
  recorder never evicts)

## Related

- `zaino-source-zcash-rpc` — parses what this returns.
- [Architecture](../../docs/architecture.md) — where this sits in the crate graph.
