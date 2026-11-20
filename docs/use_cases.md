# Indexer Live Service
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git)
2) [Zingolib](https://github.com/zingolabs/zingolib.git) [if running Zingo-Cli]

### Running ZainoD
- To run a Zaino server, backed locally by Zebrad first build Zaino.

Recently the newest GCC version on Arch has broken a build script in the `rocksdb` dependency. A workaround is:
`export CXXFLAGS="$CXXFLAGS -include cstdint"`

1) Run `$ cargo build --release`
2) Add compiled binary held at `#PATH_TO/zaino/target/release/zainod` to PATH.

- Then to launch Zaino: [in separate terminals]:
3) Run `$ zebrad --config #PATH_TO_CONF/zebrad.toml start`
4) Run `$ zainod start` (uses default config at `~/.config/zaino/zainod.toml`)
   Or with explicit config: `$ zainod start -c #PATH_TO_CONF/zainod.toml`

   To generate a default config file: `$ zainod generate-config`

NOTE: Unless `ephemeral_finalised_state` is set to true in the config file, Zaino builds its indexes from the validator it is connected to. Zaino serves as soon as its chain head has reached the validator's tip: while an index is still building, the validator answers in its place. The build resumes where it stopped if Zaino exits.

- To launch Zingo-Cli running through Zaino [from #PATH_TO/zingolib]:
5) Run `$ cargo run --release --package zingo-cli -- --chain "CHAIN_TYPE" --server "ZAINO_LISTEN_ADDR" --data-dir #PATH_TO_WALLET_DATA_DIR`

- Example Config files for running Zebra on testnet are given in `packages/zainod/`

A system architecture diagram for this service can be seen at [Live Service System Architecture](./zaino_live_system_architecture.pdf).


# Local Library
Zaino-State serves as Zaino's chain fetch and transaction submission library. The intended endpoint for this lib is the `IndexerService<Service>` (and `IndexerServiceSubscriber<ServiceSubscriber>`) held in `Zaino_state::indexer`. This generic endpoint enables zaino to add new backend options (Tonic, Darkside, Nym) to the IndexerService without changing the interface clients will see.

The use of a `Service` and `ServiceSubscriber` separates the core chainstate maintainer processes from fetch fuctionality, enabling zaino to serve a large number of concurrent clients efficiently. In the future we will also be adding a lightweight tonic backend option for clients that do not want to run any chainstate processes locally.

One `Service` is implemented, `NodeBackedIndexerService`, over one validator source in two configurations:
- JSON-RPC only (`backend = "fetch"`): compatible with any JSON-RPC validator (zebrad).
- JSON-RPC plus Zebra's read-state (`backend = "state"`): reads Zebra's state directly where Zaino and zebrad share a host.

`Zaino-State` is the legacy surface: it runs on `zaino_service::Service`, which new code builds on directly (see `packages/zaino-service/usage.md`).

Future Planned backend Services:
- TonicService: gRPC powered backend enabling lightclients and lightwieght users to use Zaino's unified chain fetch and transaction submission services.
- DarksideService: Local test backend replacing functionality in lightwalletd.
- NymService: Nym powered backend enabling clients to obfuscate their identities from zcash servers.

An example of how to spawn an `IndexerService<NodeBackedIndexerService>` and create a `Subscriber` can be seen in `packages/zainod/src/indexer.rs`.

A system architecture diagram for this service can be seen at [Library System Architecture](./zaino_lib_system_architecture.pdf).

NOTE: Currently for the mempool to function the `IndexerService` can not be dropped. An option to only keep the `Subscriber` in scope will be added with the addition of the gRPC backend (`TonicService`).

# Remote Library
**Currently Unimplemented, documentation will be added here as this functionality becomes available.**

A system architecture diagram for this service can be seen at [Library System Architecture](./zaino_lib_system_architecture.pdf).
