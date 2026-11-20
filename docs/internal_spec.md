# Zaino
The Zaino repo consists of several crates that collectively provide an indexing service and APIs for the Zcash blockchain. The crates are modularized to separate concerns, enhance maintainability, and allow for flexible integration.

### Crates
In dependency order. The source stack (`zaino-primitives` through
`zaino-source-zebra`) is described by [ADR-0008](./adr/zaino/0008-source-ports-and-domain-primitives.md);
each of its crates carries a `usage.md` beside its `Cargo.toml`.
The 27 workspace crates are listed, in dependency order, in the root
[README](../README.md#project-structure). In outline:
  - `Zaino-Logging`, `Zaino-Async`, `Zaino-Component` — logging, tasks, supervision
  - `Zaino-Consensus`, `Zaino-Primitives`, `Zaino-Address` — constants and domain vocabulary
  - `Zaino-Persistence`, `Zaino-Persistence-Lsm-Tree`, `Zaino-Indexes` — storage ports, the on-disk backend, the index set
  - `Zaino-Source` (with `Zaino-Source-Macros`), `Zaino-Rpc`, `Zaino-Source-Zcash-Rpc`, `Zaino-Source-Zebra`, `Zaino-Source-Mock` — validator ports, adapters and the test validator
  - `Zaino-Mempool`, `Zaino-Mempool-Service` — the mempool
  - `Zaino-Chain-Head`, `Zaino-Chain-Head-Service` — the non-finalised chain head
  - `Zaino-Chain-Store`, `Zaino-Chain-Store-Service` — the finalised store
  - `Zaino-Chain` — ChainView, the composed read surface
  - `Zaino-Service` — the one configurable indexer service
  - `Zaino-Proto`, `Zaino-State` (the legacy surface), `Zaino-Serve`, `Zainod`
  - `zaino-testutils`, `clientless`, `e2e` — the live-test suite (a separate workspace, `live-tests/`)

### Workspace Dependencies
The root `Cargo.toml`'s `[workspace.dependencies]` is the list; `deny.toml`
is the policy over it.


Below is a detailed specification for each crate.

A full specification of the public functionality and RPC services available in Zaino is available in [Cargo Docs](https://zingolabs.github.io/zaino/index.html) and [RPC API Spec](./rpc_api.md).


## ZainoD
`ZainoD` is the main executable that runs the Zaino indexer gRPC service. It serves as the entry point for deploying the Zaino service, handling configuration and initialization of the server components.

### Functionality
- Service Initialization:
  - Parses command-line arguments and configuration files.
  - Initializes the gRPC and JSON-RPC servers and internal caching systems using components from `zaino-serve` and `zaino-state` (backed by the `zaino-source-zebra` composite).
  - Sets up logging and monitoring systems.

- Runtime Management:
  - Manages the asynchronous runtime using `Tokio`.
  - Handles graceful shutdowns and restarts.

Full documentation for `ZainoD` can be found [here](https://zingolabs.github.io/zaino/zainod/index.html) and [here](https://zingolabs.github.io/zaino/zainodlib/index.html).


## Zaino-Serve
`Zaino-Serve` contains the gRPC server and the Rust implementations of the LightWallet gRPC service (`CompactTxStreamerServer`). It handles incoming client requests and interacts with backend services to fulfill them.

### Functionality
- gRPC Server Implementation:
  - Utilizes `Tonic` to implement the gRPC server.
  - Hosts the `CompactTxStreamerServer` service for client interactions.

- `CompactTxStreamerServer` Method Implementations:
  - Implements the full set of methods as defined in the [LightWallet Protocol](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto).

- Request Handling:
  - Validates and parses client requests.
  - Communicates with `zaino-state` to retrieve data.

- The served JSON-RPC schema (`rpc/jsonrpc/wire/`):
  - Owns the JSON shape Zaino emits for its legacy-compatible RPC surface —
    serde structs with the legacy full node's exact field names, one `from_domain` conversion
    per type, and golden serialization tests beside each.
  - This is deliberately *not* shared with the shape Zaino accepts from a
    validator, which is `zaino-source-zcash-rpc`'s. The two interfaces
    genuinely differ, and one type serving both directions cannot express that.
    See [ADR-0009](./adr/zaino/0009-served-json-schema-lives-in-zaino-serve.md).

- Error Handling:
  - Maps internal errors to appropriate gRPC status codes.
  - Recovers legacy-compatible legacy error codes by walking the error chain for
    `zaino_source::FetchError` (a code the *validator* returned) or
    `zaino_state::LegacyRpcError` (Zaino's own rejection).
  - Provides meaningful error messages to clients.

Full documentation for `Zaino-Serve` can be found [here](https://zingolabs.github.io/zaino/zaino_serve/index.html).


## Zaino-State
`Zaino-State` is Zaino's chain fetch and transaction submission library, interfacing with zcash validators throught a configurable backend. It is designed for direct consumption by full node wallets and internal services, enabling a simlified interface for Zcash clients.

### Functionality
- Blockchain Data Access:
  - Fetches finalized and non-finalized state data.
  - Retrieves transaction data and block headers.
  - Accesses chain metadata like network height and difficulty.

- Mempool Management:
  - Interfaces with the mempool to fetch pending transactions.
  - Provides efficient methods to monitor mempool changes.

- Chain Synchronization:
  - Keeps track of the chain state in sync with Zebra.
  - Handles reorgs and updates to the best chain.

- Caching Mechanisms:
  - Implements caching for frequently accessed data to improve performance.

- Configurable Backend:
  - Implementes a configurable backend service enabling clients to use a single interface for any validator set-up.

- Finalised State (persistent or ephemeral):
  - `Zaino-State` is the legacy surface over `zaino-service`. The finalised
    portion of the chain (below the chain head's window) is served by the
    chain store (`zaino-chain-store-service`): a configurable set of indexes
    on `zaino-persistence-lsm-tree`, or, when `ephemeral_finalised_state` is
    set, no store at all, the backing validator answering instead.
  - The store builds in the **background**. Startup waits only for the chain
    head's blocks to reach the validator's tip; until an index has built,
    ChainView answers its reads from the validator, capability by capability
    (the service's manifest says which).
  - A block the validator answers carries no chainwork: it is omitted, never
    served as `0`, until the store has built that far.

Full documentation for `Zaino-State` can be found [here](https://zingolabs.github.io/zaino/zaino_state/index.html).


## The source stack (`zaino-primitives` … `zaino-source-zebra`)
Validator access is a hexagonal port/adapter stack rather than a single client
library. It replaced `Zaino-Fetch`. See
[ADR-0008](./adr/zaino/0008-source-ports-and-domain-primitives.md) for the reasoning
and each crate's `usage.md` for practical guidance.

### Functionality
- Domain vocabulary (`zaino-primitives`):
  - The chain in Zaino's own terms: blocks, transactions, hashes, heights,
    treestates, amounts, and the passthrough RPC response shapes.
  - Depends on `thiserror` and nothing else. No serde: serialization belongs to
    whichever boundary owns the format.

- Decoder (`zaino-encoding`):
  - Consensus bytes to the domain block and transaction, structure checked,
    no node library. The zcash RPC adapter decodes blocks through it; the
    mempool service decodes each transaction once at ingest.

- Ports (`zaino-source`):
  - One trait per question a consumer can ask, each with its own error type.
  - `QueryError` separates a *domain answer* (the validator replied; not
    retried) from a *transport failure* (retried by `Resilient`, according to a
    machine-readable `FailureMode`). This is what lets a legacy full-node legacy error
    code survive from the validator to the served response.
  - Capability is structural: an adapter that cannot answer a question does not
    implement its port, so mis-routing is a compile error.

- Transport (`zaino-rpc`):
  - HTTP, the JSON-RPC envelope, authentication, and retry-on-`-1`.
  - `call()` returns a raw `serde_json::Value`; parsing is the adapter's job.
    This is what lets the same client serve both the production adapter and the
    live tests' independent oracle.

- Adapters (`zaino-source-zcash-rpc`, `zaino-source-zebra`'s read-state adapter):
  - The JSON-RPC adapter implements every port JSON-RPC can answer, and owns
    response parsing (Zaino's external-input validation) and error
    classification.
  - The read-state adapter reads Zebra's state database directly where Zaino
    and Zebra share a host. It is an accelerator, not an alternative, and
    deliberately does not implement the mempool or passthrough ports.

- Composite (`zaino-source-zebra`):
  - `ZebraValidator` holds an RPC adapter and an optional read-state adapter,
    and routes each question to whichever can answer it. RPC-only and
    RPC+read-state are configurations of one type rather than variants of an
    enum.


## Zaino-Proto
`Zaino-Proto` contains the `Tonic`-generated code for the LightWallet service RPCs and compact formats. It holds the protocol buffer definitions and the generated Rust code necessary for gRPC communication.

### Functionality
- Protocol Definitions:
  - `.proto` files defining the services and messages for LightWalletd APIs.
  - Includes definitions for compact blocks, transactions, and other data structures.

- Code Generation:
  - Uses `prost` to generate Rust types from `.proto` files.
  - Generates client and server stubs for gRPC services.

* We plan to eventually rely on `LibRustZcash`'s versions but hold our own here for development purposes.


## The live-test suite
`live-tests/` is a standalone workspace: `zaino-testutils` (the harness: the
validator and wallet ports, the in-process zainod launcher and the fixture
cache), `clientless` (gRPC and JSON-RPC against a real zebrad, no wallet) and
`e2e` (a wallet driving Zaino). See [testing](./testing.md) and
`live-tests/CLAUDE.md`. Unit tests in `packages/` run against one shared fake
validator, `zaino-source-mock`.
