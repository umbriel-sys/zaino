# `zainod` — usage

The binary and `zainodlib`: the daemon that runs `zaino-service` over one
validator source and serves it over gRPC and JSON-RPC. `zainod` is the one
crate that names source adapters; everything below it is built on ports.

## Configuration

One TOML file; every key can be set from the environment as
`ZAINO_<SECTION>__<KEY>` (credentials excepted). A key from the pre-v2 layout
is refused at startup with an error naming its replacement.

```toml
network = "Mainnet"                 # Mainnet | Testnet | Regtest
donation_address = "u1…"            # optional; GetLightdInfo serves it

[validator]
kind = "zcash_rpc"                  # "zcash_rpc" | "zebra"
rpc_address = "127.0.0.1:8232"
cookie_path = "/path/.cookie"       # or user + password; none sent when none set

[validator.read_state]              # kind = "zebra" only; both keys required
state_path = "/var/lib/zebra"
grpc_address = "127.0.0.1:8230"

[indexes]
set = "full"                        # lightclient | zcash_rpc | full | empty
# enable = [...], disable = [...], delete_stale = false

[persistence]
path = "/var/cache/zaino"
write_buffer_mib = 256
# threads = 4

[mempool]                           # every key optional
# max_cost_bytes, poll_interval_ms, metadata_min_interval_ms, max_exclude_count

[serve]
stream_timeout_secs = 30            # the bound on every gRPC stream
stream_channel_size = 32

[serve.grpc]
listen_address = "127.0.0.1:8137"
# tls = { cert_path = "...", key_path = "..." }

[serve.jsonrpc]                     # absent: JSON-RPC is not served
listen_address = "127.0.0.1:8237"
# cookie_dir = "/run/zaino"         # enables cookie authentication

[admin]                             # absent: no admin listener
listen_address = "127.0.0.1:9998"
```

The two validator kinds:

| `kind` | Source | Needs |
| --- | --- | --- |
| `zcash_rpc` | any zcash JSON-RPC validator | `rpc_address` |
| `zebra` | zebrad's JSON-RPC plus its read-state | `[validator.read_state]` on this host |

A zebrad reached as `zcash_rpc` is served as a generic validator: zebrad's
own refusals (its result-cap message, its wrong-network address check) are
the `zebra` kind's.

`[indexes].set = "empty"` runs without a store: every read is the
validator's.

## Serving

### gRPC limits

`[serve.grpc.limits]`, every key optional, bounds what one client connection
holds open (AP-02):

| Key | Default | Effect |
| --- | --- | --- |
| `concurrency_limit_per_connection` | 32 | requests served at once; the rest wait |
| `max_concurrent_streams` | 64 | HTTP/2 streams open at once; a further stream waits until one ends |
| `keepalive_interval_secs` | 60 | how often an idle connection is pinged |
| `keepalive_timeout_secs` | 20 | an unanswered ping closes the connection |
| `shutdown_timeout_secs` | 5 | how long a graceful shutdown waits for open requests |

An excess request queues on its connection rather than allocating, so a
client opening many streams slows itself, not the server.

### Streams

- A block range has no size cap, as lightwalletd: the bound is time. A
  stream ends with `DEADLINE_EXCEEDED` once its task has run for 4 ×
  `stream_timeout_secs` (6 × for the mempool stream).
- A stream's task stops when its client disconnects, and every stream ends
  when the server shuts down.
- `GetBlockRange` and `GetBlockRangeNullifiers` are served from encoded
  frames: each block is encoded once and the bytes are shared by every
  client through `zaino-chain`'s frame cache.

### Block headers

`zaino.lightclient.v1.LightClientService/GetBlockHeaders` takes the
lightwalletd `BlockRange` and streams `{ height, hash, header }`, the header
being the block's full serialized header. It follows `GetBlockRange`'s rules
exactly: a start above the end is served descending, and a range past the
tip is served to the tip and then refused naming the first missing height. A
header Zaino holds (the store's headers index, the chain head's window) is
served from Zaino; the validator is asked only for a height Zaino does not
hold.

```console
grpcurl -plaintext -import-path packages/zaino-proto/proto \
  -proto light_client.proto \
  -d '{"start":{"height":100},"end":{"height":110}}' \
  127.0.0.1:8137 zaino.lightclient.v1.LightClientService/GetBlockHeaders
```

### Indexed-tip subscription

`zaino.index.v1.IndexedTipService/SubscribeIndexedTips` is a server-streaming
RPC with an empty request. It emits the tip the service reads from as
`{ height, hash }`.

- The first item is the current tip, even if no block is mined after the
  client connects.
- Later items report tip changes, including same-height reorgs. A slow
  reader skips to the newest tip.
- This is not a durability notification: recent tips live in the chain
  head's in-memory window.

```console
grpcurl -plaintext -max-time 30 \
  -import-path packages/zaino-proto/proto \
  -proto indexed_tip.proto \
  127.0.0.1:8137 \
  zaino.index.v1.IndexedTipService/SubscribeIndexedTips
```

### JSON-RPC

Parameters, results and error codes follow zebrad v7; the shapes are Zaino's
own types, pinned to JSON captured from zebra-rpc. Requests may be JSON-RPC
2.0, lightwalletd's `"jsonrpc": "1.0"` or bitcoind's keyless form, and are
answered in kind.

| Failure | Code |
| --- | --- |
| a malformed or out-of-range parameter; a block not found by height | −8 |
| a bad address, txid or block hash; a block not found by hash; an unspent output for `getspentinfo` | −5 |
| bytes that are not a transaction | −22 |
| a submission the validator refuses | the validator's own (−25, −26, −27) |
| a method this deployment or its validator does not answer | −32601 |
| parameters jsonrpsee cannot decode; a start above an end | −1 (zcashd's code for invalid parameters) |
| the validator is unreachable, or Zaino failed | −32603 |

`getblockchaininfo.chainwork` is the best block's cumulative work as zcashd's
64 hex digits: Zaino's own where it holds the block, else the validator's;
the key is absent when neither is known.

Verbose `getrawtransaction` is served for a transaction in a best-chain
block; for a mempool or side-chain transaction it answers −1.

With `cookie_dir` set the server writes `<cookie_dir>/.cookie`
(`__cookie__:<secret>`, owner-only) and requires it as the Basic password.

## The library handle

`zainodlib::indexer::launch(config, Listeners)` starts the daemon in-process
and returns its task and a `ServiceHandle` (`wait_until_local`, `store_tip`,
`is_caught_up`). `Listeners` carries optional pre-bound sockets, so a caller
can bind port 0 and hand the open socket over.

## The admin listener

Own thread + current-thread runtime (a probe on the saturated serving runtime measures its queue; a
timed-out liveness probe kills the pod). Feature `prometheus`.

| Path       | Answers                                | `503` when                   |
| ---------- | -------------------------------------- | ---------------------------- |
| `/metrics` | Prometheus exposition: quantities only | never (render panic → `500`) |
| `/livez`   | indexer loop still running             | no heartbeat for 30s         |
| `/readyz`  | every component and index, as JSON     | the daemon is not answering requests |

- Heartbeat and readiness republished by the indexer loop every 100ms

### `/readyz`

`200` while the daemon answers requests: the chain head, the mempool and its
coherence layer, the gRPC server and (when configured) the JSON-RPC server
are `Ready` and neither `Critical` nor `Offline`, and the store and its
background sync have not failed. A store that is still building does not
make the daemon unready: the validator answers meanwhile. `503` otherwise,
and until the loop first reports. The body is the same either way:

```json
{
  "ready": true,
  "local": false,
  "components": [
    { "name": "Mempool", "lifecycle": "Ready", "health": "Healthy", "reason": null }
  ],
  "indexes": [
    { "name": "txid_location", "state": "building", "height": 1200000 }
  ]
}
```

| Field | Values |
| --- | --- |
| `local` | whether every answer is Zaino's own yet (nothing still building) |
| `components[].lifecycle` | `Offline`, `Spawning`, `Syncing`, `Ready`, `Closing` |
| `components[].health` | `Healthy`, `Recoverable`, `Critical`, `Offline` |
| `indexes[].state` | `absent` (not enabled), `building`, `built`; `height` is how far it serves |

## Quantities on `/metrics`, modes on `/readyz`

- Metric = a quantity (count, height, duration, size)
- Mode as a gauge (`0 none, 1 read-only, 2 full`) → averaged, `rate()`d, thresholded: meaningless,
  never an error

## Metric names

- Declared by the emitting crate's `metric_names` module: `const` names + `COUNTERS` / `GAUGES` /
  `HISTOGRAMS` (`# HELP`) tables
- `zainod` registers them and owns bucket ladders; a histogram without one fails `metrics::init`
  (would scrape as a summary)
- `metrics::init` binds the admin listener before it installs the recorder, and a bind failure
  fails startup: a recorder with no listener would record samples that nothing drains
