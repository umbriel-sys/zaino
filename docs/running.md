# Running Zaino

`zainod` is the daemon. It connects to one validator (zebrad 7 or later),
builds its indexes from it, and serves the lightwalletd gRPC protocol and a
zcash JSON-RPC subset. This page gets it running; the full configuration and
serving reference is [`zainod`'s guide](../packages/zainod/usage.md).

## Build

```sh
git clone https://github.com/umbriel-sys/zaino.git
cd zaino
cargo build --release -p zainod          # target/release/zainod
```

The Rust toolchain is pinned in `rust-toolchain.toml`. Build features:

| Feature | Effect |
| --- | --- |
| `prometheus` | the admin listener: `/metrics`, `/livez`, `/readyz` |
| `no_tls_use_unencrypted_traffic` | lets gRPC bind a public address without TLS; testing or trusted networks only |
| `allow_unencrypted_public_json_rpc_bind` | lets JSON-RPC bind a public address; only where something else encrypts the link |

## Configure

```sh
zainod generate-config                   # writes $XDG_CONFIG_HOME/zaino/zainod.toml
zainod generate-config --output ./zainod.toml
```

The generated file lists every setting with a comment
([example](./example_configs/zainod.toml)): active lines are the defaults
(Mainnet, a zebrad on this host, the `lightclient` index set, the gRPC
server), optional ones are commented out. [`zainod-full.toml`](./example_configs/zainod-full.toml)
builds every index and serves JSON-RPC as well. The settings to decide:

```toml
network = "Mainnet"                 # Mainnet | Testnet | Regtest

[validator]
kind = "zebra"                      # "zebra" | "zcash_rpc"
rpc_address = "127.0.0.1:8232"
cookie_path = "/var/cache/zebra/.cookie"

[validator.read_state]              # kind = "zebra" only
state_path = "/var/cache/zebra"
grpc_address = "127.0.0.1:8230"

[indexes]
set = "lightclient"                 # the default; lightclient | zcash_rpc | full | empty

[persistence]
path = "/var/cache/zaino"
```

- **`kind = "zebra"`** reads zebrad's state directory directly as well as
  its JSON-RPC, so zainod must run on the same host as zebrad. It is the
  faster source.
- **`kind = "zcash_rpc"`** uses JSON-RPC alone and works across hosts.
- **`[indexes].set`** chooses what Zaino answers from its own state:
  `lightclient` for wallets, `zcash_rpc` for the JSON-RPC reads, `full` for
  both, `empty` to store nothing and pass every read to the validator. A
  read whose index is not built is the validator's. `enable` adds single
  indexes to a set (`enable = ["history_tree"]`). With `delete_stale` (the
  default) an index the set no longer names is deleted from disk at start.

Every key can also come from the environment as `ZAINO_<SECTION>__<KEY>`
(for example `ZAINO_VALIDATOR__RPC_ADDRESS`). Keys ending in `password`,
`secret`, `token`, `cookie` or `private_key` are refused from the
environment: they belong in the file. A key from the pre-v2 layout is refused
at startup with an error naming its replacement.

### The validator's side

zainod needs three things from zebrad
([example](./example_configs/zebrad.toml)):

| zebrad setting | zainod setting |
| --- | --- |
| `[rpc] listen_addr` | `validator.rpc_address` |
| `[rpc] indexer_listen_addr` | `validator.read_state.grpc_address` (`zebra` kind) |
| `[state] cache_dir` | `validator.read_state.state_path` (`zebra` kind) |

zebrad enables cookie authentication by default and writes
`<cookie_dir>/.cookie`; give that path as `validator.cookie_path`, or set
`user` and `password`. With neither, zainod sends no credentials.

## Start

```sh
zainod start                             # the default config path
zainod start --config ./zainod.toml
```

zainod checks the validator is on the configured network, then serves at
once. A store that is still building does not delay serving: the validator
answers whatever the indexes do not hold yet.

Each start-up step is logged as it begins and ends, and a step still running
says so every 10 seconds:

1. `connecting to the validator`: its JSON-RPC is probed, then its network
   and activation heights are checked.
2. `opening zebra's state read-only` (`kind = "zebra"`): zebrad's database is
   opened and its write-ahead log replayed. On a large or recently synced
   node this takes a minute or more.
3. `waiting for it to reach the validator's tip`: the state and the
   JSON-RPC must name the same tip block.
4. `building the chain head`: the most recent blocks are read from the
   validator.
5. `Zaino is serving`: the listen addresses. A `Zaino status check` line
   follows every 10 seconds with each component's state, and `local = true`
   once every answer comes from Zaino's own indexes.

## Network exposure

| Listener | Config | Rule |
| --- | --- | --- |
| gRPC | `[serve.grpc]` | a public address needs TLS (`tls = { cert_path, key_path }`); without it the bind is refused at startup |
| JSON-RPC | `[serve.jsonrpc]`; absent = not served | no transport encryption: private or loopback addresses only, a public bind is refused at startup |
| Admin | `[admin]`, feature `prometheus`; absent = off | unauthenticated and unencrypted: keep it on loopback or the scraper's network |

Do not expose JSON-RPC to an untrusted network. Set
`[serve.jsonrpc].cookie_dir` to require cookie authentication.

## Logs

`RUST_LOG` sets the filter; `ZAINOLOG_FORMAT` chooses `stream`, `tree` or
`json`. See [logging](./logging.md).

## Containers

The image is built from the repository root:

```sh
podman build -f docker/Dockerfile -t zainod .
podman build -f docker/Dockerfile -t zainod --build-arg CARGO_FEATURES=prometheus .
```

| Build arg | Values | Default |
| --- | --- | --- |
| `CARGO_FEATURES` | comma-separated features | none |
| `CARGO_PROFILE` | `release`, `profiling` (frame pointers and line tables) | `release` |
| `ZAINO_GIT_COMMIT_ID`, `ZAINO_GIT_BRANCH` | reported by `zainod --version` | `unknown` |

The container runs `zainod start` as a non-root user (UID 1000) and refuses
to start as root. It reads its config from `/app/config/zainod.toml` and
keeps its store in `/app/data`:

```sh
podman run --rm --userns=keep-id \
  -p 8137:8137 \
  -v ./zainod.toml:/app/config/zainod.toml:ro,Z \
  -v zaino-data:/app/data \
  zainod
```

`--userns=keep-id` maps the container's user to yours, so files in the data
volume stay owned by you. With the `zebra` validator kind the container also
needs zebrad's state directory mounted at `validator.read_state.state_path`.

To write a starting config with the image, mount a config directory writable
by UID 1000 and run `generate-config`; the file appears as
`./config/zainod.toml`:

```sh
mkdir -p ./config
podman run --rm --userns=keep-id -v ./config:/app/config:Z zainod generate-config
```

## Nix

The repository is a flake:

```sh
nix build .#zainod                       # ./result/bin/zainod
nix develop                              # a shell with the pinned toolchain and build tools
```

## As a library

`zainodlib::indexer::launch(config, Listeners)` starts the daemon in-process
and returns its task and a handle
([details](../packages/zainod/usage.md#the-library-handle)). To build on the
service without the servers, use
[`zaino-service`](../packages/zaino-service/usage.md).
