# Zaino

Zaino is an indexer for the Zcash blockchain, written in Rust.

It sits between a full validator ([Zebra](https://github.com/ZcashFoundation/zebra))
and client software. It reads the chain from the validator, builds its own
indexes, and serves:

- **light clients** through the
  [lightwallet protocol](https://github.com/zcash/lightwallet-protocol) gRPC
  service that [lightwalletd](https://github.com/zcash/lightwalletd) serves
  today, and
- **full clients and block explorers** through the subset of the
  [zcash JSON-RPC](https://zcash.github.io/rpc/) they need.

Keeping indexing out of the validator keeps the consensus node small and puts
a clear trust boundary between validating the chain and serving clients.

## Start here

| To | Read |
| --- | --- |
| run `zainod` | [docs/running.md](./docs/running.md) |
| see what is served, and from where | [docs/rpc_api.md](./docs/rpc_api.md) |
| understand how the crates fit together | [docs/architecture.md](./docs/architecture.md) |
| run the tests | [docs/testing.md](./docs/testing.md) |
| contribute | [CONTRIBUTING.md](./CONTRIBUTING.md) |

```sh
cargo build --release -p zainod
./target/release/zainod generate-config --output ./zainod.toml   # then edit it
./target/release/zainod start --config ./zainod.toml
```

Zaino is in development; test it before trusting it with a mainnet service.

## Repository layout

```
packages/        the 27 workspace crates (see "Crate usage guides")
live-tests/      the live suite: a standalone workspace over a real zebrad
  e2e/             a wallet drives Zaino end to end
  clientless/      Zaino's gRPC and JSON-RPC against a live validator, no wallet
  zaino-testutils/ the harness and the fixture cache
tools/           development tools, each its own workspace
  workbench/       repository guards run by `makers lint`
  relman/          the changeset and release tool
  zaino-bench/     chain-store benchmarks
docs/            repository documentation
docker/          the container image: Dockerfile and entrypoint
nix/             the Nix package (flake.nix is at the root)
.changesets/     one file per change; changelogs are generated from them
.release/        the ledger of changesets already released
.github/         CI workflows and issue templates
.githooks/       the pre-push hook (`makers pre-push`)
```

Root files: `Cargo.toml` / `Cargo.lock` (the workspace), `rust-toolchain.toml`
(the pinned toolchain), `Makefile.toml` (cargo-make tasks), `deny.toml`
(cargo-deny bans), `relman.toml` (release targets), `.dupes-ignore.toml` (the
duplication guard's allow-list), `flake.nix` / `flake.lock`, `.dockerignore`,
`CHANGELOG.md` (generated), `CLAUDE.md` (coding rules; `AGENTS.md` links to
it), `CONTRIBUTING.md`, `LICENSE`.

## Documentation

- [Architecture](./docs/architecture.md): the layers, where an answer comes from, the crates.
- [Running Zaino](./docs/running.md): build, configure, start; containers and Nix.
- [Served RPCs](./docs/rpc_api.md): every gRPC and JSON-RPC method and its data path.
- [Logging](./docs/logging.md): the log variables and formats.
- [Testing](./docs/testing.md): the unit suite, the live suite and its fixture.
- [Live-test guidelines](./live-tests/CLAUDE.md): the rules for writing live tests.
- [Changesets and releases](./docs/release.md): recording a change, cutting a release.
- [Updating Zebra crates](./docs/updating_zebra_crates.md): the bump procedure.
- [Glossary](./docs/glossary.md): the project's terms.
- Example configs: [`zainod.toml`](./docs/example_configs/zainod.toml), [`zebrad.toml`](./docs/example_configs/zebrad.toml).

### Crate usage guides

Practical guidance for working *in* a crate: its scope, its invariants, and
the mistakes its design is trying to prevent.

- [`zaino-logging`](./packages/zaino-logging/usage.md): centralized tracing setup, and the panic-at-origin hook that routes panics through the same sink.
- [`zaino-async`](./packages/zaino-async/usage.md): the low-level async/tokio primitives (named, panic-rendering `Task`) the component layer is built on.
- [`zaino-component`](./packages/zaino-component/usage.md): the component abstraction, its two independent axes, and the observed/owned line.
- [`zaino-consensus`](./packages/zaino-consensus/usage.md): the protocol constants, and why they are stated rather than borrowed.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): the domain vocabulary, and why it depends on nothing.
- [`zaino-encoding`](./packages/zaino-encoding/usage.md): the consensus-bytes decoder, and what it does not check.
- [`zaino-address`](./packages/zaino-address/usage.md): address classification, and what is not classified.
- [`zaino-persistence`](./packages/zaino-persistence/usage.md): the storage backend port, and why index code never names a concrete store.
- [`zaino-persistence-lsm-tree`](./packages/zaino-persistence-lsm-tree/usage.md): the on-disk backend, how to open and tune it, and what a failed write or merge leaves behind.
- [`zaino-indexes`](./packages/zaino-indexes/usage.md): per-field record layouts, index schemas and the digest a directory is stamped with.
- [`zaino-source-macros`](./packages/zaino-source-macros/usage.md): the macro that derives each resilient port from its one-shot twin.
- [`zaino-source`](./packages/zaino-source/usage.md): the ports, the domain/fetch error split, and `Resilient`.
- [`zaino-source-mock`](./packages/zaino-source-mock/usage.md): the one fake validator every crate tests against, and the contract that keeps it honest.
- [`zaino-rpc`](./packages/zaino-rpc/usage.md): JSON-RPC transport, and what it does not do.
- [`zaino-source-zcash-rpc`](./packages/zaino-source-zcash-rpc/usage.md): the JSON-RPC adapter, its error classification, and the quirk table that keeps it validator-agnostic.
- [`zaino-source-zebra`](./packages/zaino-source-zebra/usage.md): the composite and its routing rules, the read-state adapter and what it cannot answer, and zebrad's quirks.
- [`zaino-mempool`](./packages/zaino-mempool/usage.md): the two-layer model, the ports, and the bounds.
- [`zaino-mempool-service`](./packages/zaino-mempool-service/usage.md): spawning and consuming the mempool.
- [`zaino-proto`](./packages/zaino-proto/usage.md): the light-wallet types, and the compact-block conversion and direct writer every serving crate shares.
- [`zaino-chain-head`](./packages/zaino-chain-head/usage.md): the chain head's ports, why reads live on the snapshot, and why there is no way to make it synchronise.
- [`zaino-chain-head-service`](./packages/zaino-chain-head-service/usage.md): the chain head runtime, its two testing styles, and the properties to keep when editing the advance path.
- [`zaino-chain-store`](./packages/zaino-chain-store/usage.md): the finalised state's ports, why the chunk is the block-read primitive, and why a read past the watermark is not a miss.
- [`zaino-chain-store-service`](./packages/zaino-chain-store-service/usage.md): the chain store runtime: configuring the index set, what boot checks and repairs, and `build_to`.
- [`zaino-chain`](./packages/zaino-chain/usage.md): ChainView's ports, how coverage decides which provider answers, and what a hole between the tiers degrades versus what it refuses.
- [`zaino-service`](./packages/zaino-service/usage.md): the one configurable service, its store modes, and the manifest of who answers each capability.
- [`zaino-state`](./packages/zaino-state/usage.md): the legacy indexer surface on the service, and its legacy vocabulary.
- [`zainod`](./packages/zainod/usage.md): the daemon: configuration, the two validator sources, the served gRPC and JSON-RPC surfaces (headers and history proofs included), and the admin listener (`/metrics`, `/livez`, `/readyz`).

## Security

Do not report a vulnerability in a public issue. Report it privately through
the repository's
[security advisories](https://github.com/umbriel-sys/zaino/security).

## License

Licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0).
See [LICENSE](./LICENSE).
