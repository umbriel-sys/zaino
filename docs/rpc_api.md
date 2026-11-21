# Served RPCs

Every method `zainod` serves, and where its answer comes from. Parameters,
results, error codes and limits are in
[`zainod`'s guide](../packages/zainod/usage.md#serving).

Where an answer comes from:

- **Zaino**: Zaino's own state. For a chain read that means the indexes of
  the chain store and the chain head. Without the index that serves it (see
  `[indexes]`), or at a height the store has not built yet, the validator
  answers instead; nothing is refused for being unbuilt.
- **Mempool**: Zaino's mirror of the validator's mempool.
- **Validator**: passed through; Zaino keeps nothing for it.

## gRPC: `cash.z.wallet.sdk.rpc.CompactTxStreamer`

The [lightwallet protocol](https://github.com/zcash/lightwallet-protocol),
v0.6.0 (`LightdInfo.lightwalletProtocolVersion`).

| Method | From |
| --- | --- |
| `GetLatestBlock` | Zaino |
| `GetBlock`, `GetBlockNullifiers` | Zaino |
| `GetBlockRange`, `GetBlockRangeNullifiers` | Zaino, as encoded frames shared between clients |
| `GetTransaction` | Zaino places the transaction (chain or mempool); bytes from the mempool, else the validator |
| `SendTransaction` | Validator |
| `GetTaddressTxids`, `GetTaddressTransactions` | Zaino for the txids; each transaction as `GetTransaction` |
| `GetTaddressBalance`, `GetTaddressBalanceStream` | Zaino |
| `GetAddressUtxos`, `GetAddressUtxosStream` | Zaino |
| `GetMempoolTx`, `GetMempoolStream` | Mempool |
| `GetTreeState`, `GetLatestTreeState` | Zaino |
| `GetSubtreeRoots` | Zaino |
| `GetLightdInfo` | Validator, with Zaino's tip and build |
| `GetStandardFee` | Validator; `UNIMPLEMENTED` when it lacks `getstandardfee` |
| `Ping` | never enabled: `FAILED_PRECONDITION`, as lightwalletd answers when it is off |

## gRPC: Zaino's own services

| Method | From |
| --- | --- |
| `zaino.lightclient.v1.LightClientService/GetBlockHeaders` | Zaino |
| `zaino.lightclient.v1.LightClientService/GetHistoryProof` | Zaino (the `history_tree` index; `UNIMPLEMENTED` without it) |
| `zaino.index.v1.IndexedTipService/SubscribeIndexedTips` | Zaino |

## JSON-RPC

Shapes and codes follow zebrad 7.

| Method | From |
| --- | --- |
| `getbestblockhash`, `getblockcount`, `getchaintips` | Zaino |
| `getblockhash`, `getblockhashes` | Zaino |
| `getblockheader` | Zaino |
| `getblock` | Validator, at the hash Zaino holds for the block asked |
| `getblockdeltas` | Zaino; the validator where previous outputs are not local |
| `getrawtransaction` | Validator, only for a transaction Zaino places (chain or mempool); raw bytes from the mempool where it holds them |
| `gettxout` | Validator |
| `getspentinfo` | Zaino, then the mempool, then the validator |
| `gettxoutsetinfo` | Zaino (the `tx_out_set` index; −32601 without it) |
| `getaddressbalance`, `getaddresstxids`, `getaddressutxos`, `getaddressdeltas` | Zaino |
| `z_gettreestate`, `z_getsubtreesbyindex` | Zaino |
| `getrawmempool`, `getmempoolinfo` | Mempool |
| `sendrawtransaction` | Validator |
| `validateaddress`, `z_validateaddress` | Zaino (no chain data) |
| `getblockchaininfo` | Validator, with Zaino's chainwork where it holds the block |
| `getinfo`, `getnetworkinfo`, `getpeerinfo`, `ping` | Validator |
| `getmininginfo`, `getnetworksolps`, `getdifficulty`, `getblocksubsidy`, `getstandardfee` | Validator |

A method the validator does not answer is −32601.
