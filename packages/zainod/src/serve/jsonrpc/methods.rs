//! The JSON-RPC methods over [`Service`](zaino_service::Service), in Zaino's
//! own types: zebrad v7's parameters, results and error codes.

mod errors;
mod params;
#[cfg(test)]
mod tests;

use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use zaino_chain::{AddressRead as _, BlockId, BlockRead as _, ForkReconcile as _, TreestateRead};
use zaino_primitives::types::rpc::SubtreeRoots;
use zaino_primitives::types::{
    BlockTransactions, BlockVerbosity, Height, MempoolInfo, TransactionId,
};
use zaino_service::{Placement, ServiceError, ServiceSnapshot, ServiceSource};
use zaino_source::{SolutionRateWindow, UtxoPage};

use self::errors::{
    chain_error, internal, not_served, rpc_error, service_error, DESERIALIZATION,
    INVALID_ADDRESS_OR_KEY, INVALID_PARAMETER, MISC,
};
use super::wire::address::{AddressNetwork, ZValidateAddressWire};
use super::wire::address_deltas::{
    GetAddressDeltasError, GetAddressDeltasParams, GetAddressDeltasResponse,
};
use super::wire::block::{GetBlockWire, RawTransactionWire};
use super::wire::block_deltas::BlockDeltas;
use super::wire::block_header::GetBlockHeader;
use super::wire::block_subsidy::GetBlockSubsidy;
use super::wire::chain_tips::{chain_tips_from_domain, GetChainTipsResponse};
use super::wire::display_hex;
use super::wire::mining_info::GetMiningInfoWire;
use super::wire::misc::{
    MempoolInfoWire, NetworkSolPsWire, SpentInfoRequestWire, SpentInfoWire, TxOutSetInfoWire,
    TxOutWire,
};
use super::wire::peer_info::GetPeerInfo;
use super::wire::served::{
    block_hashes_from_domain, AddressBalanceWire, AddressTxIdsRequestWire, AddressUtxosRequestWire,
    AddressUtxosWire, AddressesError, AddressesRequestWire, BlockHashesOptionsWire,
    BlockchainInfoWire, GetInfoWire, NetworkInfoWire, SubtreesWire, TreestateWire,
    ValidateAddressWire, MAX_REQUEST_ADDRESSES,
};
use super::wire::standard_fee::GetStandardFee;
use crate::serve::backend::Backend;

/// The served JSON-RPC interface.
#[rpc(server)]
trait ZainoRpc {
    /// The validator's node info.
    #[method(name = "getinfo")]
    async fn get_info(&self) -> RpcResult<GetInfoWire>;

    /// The validator's chain info, with the best block's cumulative work.
    #[method(name = "getblockchaininfo")]
    async fn get_blockchain_info(&self) -> RpcResult<BlockchainInfoWire>;

    /// The validator's network view.
    #[method(name = "getnetworkinfo")]
    async fn get_network_info(&self) -> RpcResult<NetworkInfoWire>;

    /// Whether the validator answers.
    #[method(name = "ping")]
    async fn ping(&self) -> RpcResult<()>;

    /// The mempool's size and cost.
    #[method(name = "getmempoolinfo")]
    async fn get_mempool_info(&self) -> RpcResult<MempoolInfoWire>;

    /// The validator's mining info.
    #[method(name = "getmininginfo")]
    async fn get_mining_info(&self) -> RpcResult<GetMiningInfoWire>;

    /// The unspent output set's statistics at the tip.
    #[method(name = "gettxoutsetinfo")]
    async fn get_tx_out_set_info(&self) -> RpcResult<TxOutSetInfoWire>;

    /// The tip's hash.
    #[method(name = "getbestblockhash")]
    async fn get_best_block_hash(&self) -> RpcResult<String>;

    /// The tip's height.
    #[method(name = "getblockcount")]
    async fn get_block_count(&self) -> RpcResult<u32>;

    /// The best-chain hash at `index`; negative counts back from the tip.
    #[method(name = "getblockhash")]
    async fn get_block_hash(&self, index: i32) -> RpcResult<String>;

    /// The best-chain blocks timed in `[low, high)`.
    #[method(name = "getblockhashes")]
    async fn get_block_hashes(
        &self,
        high: u32,
        low: u32,
        options: Option<BlockHashesOptionsWire>,
    ) -> RpcResult<Vec<String>>;

    /// The validator's tip difficulty.
    #[method(name = "getdifficulty")]
    async fn get_difficulty(&self) -> RpcResult<f64>;

    /// A block's transparent deltas.
    #[method(name = "getblockdeltas")]
    async fn get_block_deltas(&self, hash: String) -> RpcResult<BlockDeltas>;

    /// The validator's peers.
    #[method(name = "getpeerinfo")]
    async fn get_peer_info(&self) -> RpcResult<GetPeerInfo>;

    /// The block subsidy at `height`, the tip's when absent.
    #[method(name = "getblocksubsidy")]
    async fn get_block_subsidy(&self, height: Option<u32>) -> RpcResult<GetBlockSubsidy>;

    /// The validator's standard fee.
    #[method(name = "getstandardfee")]
    async fn get_standard_fee(&self) -> RpcResult<GetStandardFee>;

    /// Every chain tip Zaino holds.
    #[method(name = "getchaintips")]
    async fn get_chain_tips(&self) -> RpcResult<GetChainTipsResponse>;

    /// Whether `address` is a valid transparent address.
    #[method(name = "validateaddress")]
    async fn validate_address(&self, address: String) -> RpcResult<ValidateAddressWire>;

    /// `address`'s kind, any pool.
    #[method(name = "z_validateaddress")]
    async fn z_validate_address(&self, address: String) -> RpcResult<ZValidateAddressWire>;

    /// The addresses' balance and lifetime receipts.
    #[method(name = "getaddressbalance")]
    async fn get_address_balance(
        &self,
        address_strings: AddressesRequestWire,
    ) -> RpcResult<AddressBalanceWire>;

    /// Submits a transaction; answers its id.
    #[method(name = "sendrawtransaction")]
    async fn send_raw_transaction(&self, raw_transaction_hex: String) -> RpcResult<String>;

    /// A block: its hex at verbosity 0, else its object.
    #[method(name = "getblock")]
    async fn get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> RpcResult<GetBlockWire>;

    /// A block's header: its object, or its hex when not `verbose`.
    #[method(name = "getblockheader")]
    async fn get_block_header(
        &self,
        hash_or_height: String,
        verbose: Option<bool>,
    ) -> RpcResult<GetBlockHeader>;

    /// The mempool's transaction ids.
    #[method(name = "getrawmempool")]
    async fn get_raw_mempool(&self) -> RpcResult<Vec<String>>;

    /// A block's note commitment trees.
    #[method(name = "z_gettreestate")]
    async fn z_get_treestate(&self, hash_or_height: String) -> RpcResult<TreestateWire>;

    /// A pool's subtree roots from `start_index`.
    #[method(name = "z_getsubtreesbyindex")]
    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> RpcResult<SubtreesWire>;

    /// A transaction: its hex, or its object when `verbose` is not 0.
    #[method(name = "getrawtransaction")]
    async fn get_raw_transaction(
        &self,
        txid: String,
        verbose: Option<u8>,
        block_hash: Option<String>,
    ) -> RpcResult<RawTransactionWire>;

    /// An unspent output, `null` when spent or unknown.
    #[method(name = "gettxout")]
    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> RpcResult<TxOutWire>;

    /// Where an output was spent.
    #[method(name = "getspentinfo")]
    async fn get_spent_info(&self, request: SpentInfoRequestWire) -> RpcResult<SpentInfoWire>;

    /// The ids of the transactions touching the addresses.
    #[method(name = "getaddresstxids")]
    async fn get_address_tx_ids(&self, request: AddressTxIdsRequestWire) -> RpcResult<Vec<String>>;

    /// The addresses' unspent outputs.
    #[method(name = "getaddressutxos")]
    async fn get_address_utxos(
        &self,
        address_strings: AddressUtxosRequestWire,
    ) -> RpcResult<AddressUtxosWire>;

    /// The addresses' balance changes.
    #[method(name = "getaddressdeltas")]
    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> RpcResult<GetAddressDeltasResponse>;

    /// The validator's network solutions per second.
    #[method(name = "getnetworksolps")]
    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> RpcResult<NetworkSolPsWire>;
}

/// The methods' implementation over the service.
struct JsonRpcMethods<S: ServiceSource> {
    backend: Backend<S>,
}

/// Every served method, registered.
pub fn methods<S: ServiceSource + 'static>(backend: Backend<S>) -> jsonrpsee::Methods {
    JsonRpcMethods { backend }.into_rpc().into()
}

/// zebrad's answer for a block it does not hold: −5 by hash, −8 by height.
fn block_not_in_best_chain(at: BlockId) -> ErrorObjectOwned {
    let code = match at {
        BlockId::Hash(_) => INVALID_ADDRESS_OR_KEY,
        BlockId::Height(_) => INVALID_PARAMETER,
    };
    rpc_error(code, "block height not in best chain")
}

/// zebrad's codes for a refused address list: −8 for its length, −5 for an
/// address.
fn addresses_error(error: AddressesError) -> ErrorObjectOwned {
    let code = match error {
        AddressesError::TooMany => INVALID_PARAMETER,
        AddressesError::Tex | AddressesError::Invalid { .. } => INVALID_ADDRESS_OR_KEY,
    };
    rpc_error(code, error)
}

fn no_such_transaction() -> ErrorObjectOwned {
    rpc_error(
        INVALID_ADDRESS_OR_KEY,
        "No such mempool or main chain transaction",
    )
}

/// A transaction id, or zebrad's −5.
fn txid_or_invalid_key(hex: &str) -> RpcResult<TransactionId> {
    params::txid(hex).ok_or_else(|| {
        rpc_error(
            INVALID_ADDRESS_OR_KEY,
            "txid must be 64 hexadecimal characters",
        )
    })
}

impl<S: ServiceSource> JsonRpcMethods<S> {
    fn snapshot(&self) -> ServiceSnapshot<S> {
        self.backend.service.snapshot()
    }

    fn address_network(&self) -> AddressNetwork {
        AddressNetwork(self.backend.info.network)
    }
}

/// zebrad's hash-or-height against `snapshot`'s tip, or its −8.
fn block_id<S: ServiceSource>(snapshot: &ServiceSnapshot<S>, input: &str) -> RpcResult<BlockId> {
    params::hash_or_height(input, snapshot.tip().height).ok_or_else(|| {
        rpc_error(
            INVALID_PARAMETER,
            "parse error: could not convert the input string to a hash or height",
        )
    })
}

/// A verbose `getrawtransaction` object. The service decodes no transaction,
/// so the object is the validator's, from the best-chain block holding it.
async fn transaction_object<S: ServiceSource>(
    snapshot: &ServiceSnapshot<S>,
    txid: TransactionId,
    block: Option<zaino_primitives::types::BlockHash>,
) -> RpcResult<RawTransactionWire> {
    let undecodable = |place: &str| {
        rpc_error(
            MISC,
            format!("the transaction is {place}: its verbose form is served only from a best-chain block"),
        )
    };
    let status = snapshot
        .transaction_status(txid)
        .await
        .map_err(service_error)?;
    let position = match status.best {
        Some(Placement::Block(position)) => position,
        Some(Placement::Mempool(_)) => return Err(undecodable("in the mempool")),
        None if status.elsewhere.is_empty() => return Err(no_such_transaction()),
        None => return Err(undecodable("off the best chain")),
    };
    if block.is_some_and(|block| block != position.block.hash) {
        return Err(rpc_error(INVALID_ADDRESS_OR_KEY, "txid not found"));
    }
    let holder = snapshot
        .block_verbose(
            BlockId::Hash(position.block.hash),
            BlockVerbosity::Transactions,
        )
        .await
        .map_err(service_error)?
        .ok_or_else(|| internal(ServiceError::MissingBlock(position.block.height)))?;
    let BlockTransactions::Objects(transactions) = &holder.transactions else {
        return Err(internal("the validator's verbose block lists no objects"));
    };
    let transaction = transactions
        .iter()
        .find(|transaction| transaction.txid == txid)
        .ok_or_else(|| internal("the validator's block omits a transaction Zaino places in it"))?;
    RawTransactionWire::object_from_domain(transaction).map_err(internal)
}

#[jsonrpsee::core::async_trait]
impl<S: ServiceSource + 'static> ZainoRpcServer for JsonRpcMethods<S> {
    async fn get_info(&self) -> RpcResult<GetInfoWire> {
        self.snapshot()
            .node_info()
            .await
            .map(GetInfoWire::from_domain)
            .map_err(service_error)
    }

    async fn get_blockchain_info(&self) -> RpcResult<BlockchainInfoWire> {
        let snapshot = self.snapshot();
        let info = snapshot.blockchain_info().await.map_err(service_error)?;
        // the work of the very block reported, where Zaino holds it
        let chainwork = snapshot
            .chain()
            .block(BlockId::Hash(info.best_block_hash))
            .await
            .map_err(chain_error)?
            .and_then(|block| block.chainwork);
        BlockchainInfoWire::from_domain(info, chainwork).map_err(internal)
    }

    async fn get_network_info(&self) -> RpcResult<NetworkInfoWire> {
        self.snapshot()
            .network_info()
            .await
            .map(NetworkInfoWire::from_domain)
            .map_err(service_error)
    }

    async fn ping(&self) -> RpcResult<()> {
        self.snapshot().ping().await.map_err(service_error)
    }

    async fn get_mempool_info(&self) -> RpcResult<MempoolInfoWire> {
        let info = self.snapshot().mempool_info();
        Ok(MempoolInfoWire::from_domain(MempoolInfo {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }))
    }

    async fn get_mining_info(&self) -> RpcResult<GetMiningInfoWire> {
        self.snapshot()
            .mining_info()
            .await
            .map(GetMiningInfoWire::from_domain)
            .map_err(service_error)
    }

    async fn get_tx_out_set_info(&self) -> RpcResult<TxOutSetInfoWire> {
        self.snapshot()
            .txout_set_info()
            .await
            .map(TxOutSetInfoWire::from_domain)
            .map_err(service_error)
    }

    async fn get_best_block_hash(&self) -> RpcResult<String> {
        Ok(display_hex(self.snapshot().tip().hash.into()))
    }

    async fn get_block_count(&self) -> RpcResult<u32> {
        Ok(self.snapshot().tip().height.into())
    }

    async fn get_block_hash(&self, index: i32) -> RpcResult<String> {
        self.snapshot()
            .block_hash(index)
            .await
            .map(|hash| display_hex(hash.into()))
            .map_err(|error| match error {
                // zebrad's answer for a best-chain height without a block
                ServiceError::MissingBlock(_) => rpc_error(INVALID_PARAMETER, "Block not found"),
                error => service_error(error),
            })
    }

    async fn get_block_hashes(
        &self,
        high: u32,
        low: u32,
        options: Option<BlockHashesOptionsWire>,
    ) -> RpcResult<Vec<String>> {
        if options.is_some_and(|options| options.logical_times) {
            return Err(rpc_error(
                INVALID_PARAMETER,
                "logicalTimes is not served: Zaino indexes no logical timestamps",
            ));
        }
        self.snapshot()
            .block_hashes(low, high)
            .await
            .map(block_hashes_from_domain)
            .map_err(service_error)
    }

    async fn get_difficulty(&self) -> RpcResult<f64> {
        self.snapshot()
            .difficulty_at_tip()
            .await
            .map_err(service_error)
    }

    async fn get_block_deltas(&self, hash: String) -> RpcResult<BlockDeltas> {
        let hash = params::block_hash(&hash).ok_or_else(|| {
            rpc_error(
                INVALID_PARAMETER,
                "blockhash must be 64 hexadecimal characters",
            )
        })?;
        self.snapshot()
            .block_deltas(hash)
            .await
            .map_err(service_error)?
            .map(BlockDeltas::from_domain)
            .ok_or_else(|| rpc_error(INVALID_PARAMETER, format!("block {hash} not found")))
    }

    async fn get_peer_info(&self) -> RpcResult<GetPeerInfo> {
        self.snapshot()
            .peer_info()
            .await
            .map(GetPeerInfo::from_domain)
            .map_err(service_error)
    }

    async fn get_block_subsidy(&self, height: Option<u32>) -> RpcResult<GetBlockSubsidy> {
        let height = height
            .map(Height::try_from)
            .transpose()
            .map_err(|error| rpc_error(INVALID_PARAMETER, error))?;
        self.snapshot()
            .block_subsidy(height)
            .await
            .map(GetBlockSubsidy::from_domain)
            .map_err(service_error)
    }

    async fn get_standard_fee(&self) -> RpcResult<GetStandardFee> {
        self.snapshot()
            .standard_fee()
            .await
            .map(GetStandardFee::from_domain)
            .map_err(service_error)
    }

    async fn get_chain_tips(&self) -> RpcResult<GetChainTipsResponse> {
        Ok(chain_tips_from_domain(self.snapshot().chain().chain_tips()))
    }

    async fn validate_address(&self, address: String) -> RpcResult<ValidateAddressWire> {
        Ok(ValidateAddressWire::from_domain(
            zaino_address::validate_address(address, &self.address_network()),
        ))
    }

    async fn z_validate_address(&self, address: String) -> RpcResult<ZValidateAddressWire> {
        tracing::warn!("{}", zaino_address::DEPRECATION_NOTICE);
        Ok(ZValidateAddressWire::from_domain(
            zaino_address::z_validate_address(address, &self.address_network()),
        ))
    }

    async fn get_address_balance(
        &self,
        address_strings: AddressesRequestWire,
    ) -> RpcResult<AddressBalanceWire> {
        let addresses = address_strings.into_domain().map_err(addresses_error)?;
        let balance = self
            .snapshot()
            .chain()
            .address_balance(&addresses)
            .await
            .map_err(chain_error)?;
        AddressBalanceWire::from_domain(balance).map_err(internal)
    }

    async fn send_raw_transaction(&self, raw_transaction_hex: String) -> RpcResult<String> {
        // refused before the validator round trip, with zebrad's −22
        let bytes =
            hex::decode(&raw_transaction_hex).map_err(|error| rpc_error(DESERIALIZATION, error))?;
        self.snapshot()
            .send_raw_transaction(bytes)
            .await
            .map(|txid| display_hex(txid.into()))
            .map_err(|error| match error {
                ServiceError::InvalidParameter(message) => rpc_error(DESERIALIZATION, message),
                error => service_error(error),
            })
    }

    async fn get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> RpcResult<GetBlockWire> {
        let snapshot = self.snapshot();
        let at = block_id(&snapshot, &hash_or_height)?;
        let level = verbosity.unwrap_or(1);
        if level == 0 {
            return snapshot
                .raw_block(at)
                .await
                .map_err(service_error)?
                .map(|bytes| GetBlockWire::raw_from_domain(&bytes))
                .ok_or_else(|| rpc_error(INVALID_PARAMETER, "Block not found"));
        }
        let verbosity = BlockVerbosity::from_level(level)
            .ok_or_else(|| rpc_error(INVALID_PARAMETER, "invalid verbosity value"))?;
        let block = snapshot
            .block_verbose(at, verbosity)
            .await
            .map_err(service_error)?
            .ok_or_else(|| block_not_in_best_chain(at))?;
        GetBlockWire::object_from_domain(&block).map_err(internal)
    }

    async fn get_block_header(
        &self,
        hash_or_height: String,
        verbose: Option<bool>,
    ) -> RpcResult<GetBlockHeader> {
        let snapshot = self.snapshot();
        let at = block_id(&snapshot, &hash_or_height)?;
        let header = match verbose.unwrap_or(true) {
            true => snapshot
                .block_header(at)
                .await
                .map_err(service_error)?
                .map(GetBlockHeader::verbose_from_domain),
            false => snapshot
                .raw_block_header(at)
                .await
                .map_err(service_error)?
                .map(GetBlockHeader::compact_from_domain),
        };
        header.ok_or_else(|| block_not_in_best_chain(at))
    }

    async fn get_raw_mempool(&self) -> RpcResult<Vec<String>> {
        Ok(self
            .snapshot()
            .mempool_txids()
            .iter()
            .map(|txid| display_hex((*txid).into()))
            .collect())
    }

    async fn z_get_treestate(&self, hash_or_height: String) -> RpcResult<TreestateWire> {
        let snapshot = self.snapshot();
        let at = block_id(&snapshot, &hash_or_height)?;
        snapshot
            .chain()
            .treestate(at)
            .await
            .map_err(chain_error)?
            .map(TreestateWire::from_domain)
            .ok_or_else(|| {
                rpc_error(
                    INVALID_PARAMETER,
                    "the requested block is not in the main chain",
                )
            })
    }

    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> RpcResult<SubtreesWire> {
        // zebrad's −1 for a pool it does not know
        let pool = super::wire::subtrees::pool_into_domain(&pool)
            .map_err(|error| rpc_error(MISC, error))?;
        let subtrees = self
            .snapshot()
            .chain()
            .subtree_roots(pool, start_index, limit)
            .await
            .map_err(chain_error)?;
        Ok(SubtreesWire::from_domain(SubtreeRoots {
            pool,
            start_index,
            subtrees,
        }))
    }

    async fn get_raw_transaction(
        &self,
        txid: String,
        verbose: Option<u8>,
        block_hash: Option<String>,
    ) -> RpcResult<RawTransactionWire> {
        let txid = txid_or_invalid_key(&txid)?;
        let block = block_hash
            .map(|hash| {
                params::block_hash(&hash).ok_or_else(|| {
                    rpc_error(
                        INVALID_ADDRESS_OR_KEY,
                        "blockhash must be 64 hexadecimal characters",
                    )
                })
            })
            .transpose()?;
        let snapshot = self.snapshot();
        if verbose.unwrap_or(0) != 0 {
            return transaction_object(&snapshot, txid, block).await;
        }
        let bytes = match block {
            Some(block) => snapshot
                .raw_transaction(txid, Some(block))
                .await
                .map_err(service_error)?
                .map(|transaction| transaction.bytes),
            None => snapshot
                .transaction(txid)
                .await
                .map_err(service_error)?
                .map(|transaction| transaction.bytes),
        };
        bytes
            .map(|bytes| RawTransactionWire::raw_from_domain(&bytes))
            .ok_or_else(no_such_transaction)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> RpcResult<TxOutWire> {
        // zebrad's −8 for a malformed txid
        let txid = params::txid(&txid).ok_or_else(|| {
            rpc_error(INVALID_PARAMETER, "txid must be 64 hexadecimal characters")
        })?;
        self.snapshot()
            .tx_out(txid, n, include_mempool.unwrap_or(true))
            .await
            .map(TxOutWire::from_domain)
            .map_err(service_error)
    }

    async fn get_spent_info(&self, request: SpentInfoRequestWire) -> RpcResult<SpentInfoWire> {
        let outpoint = request
            .into_domain()
            .map_err(|error| rpc_error(INVALID_PARAMETER, error))?;
        self.snapshot()
            .spent_info(outpoint)
            .await
            .map_err(|error| match error {
                // the validator's own answer where it lacks the method
                ServiceError::NotServed { .. } => {
                    not_served(zaino_source::GetSpentInfoError::Unsupported)
                }
                error => service_error(error),
            })?
            .map(SpentInfoWire::from_domain)
            // zcashd's −5 for an unspent output
            .ok_or_else(|| {
                rpc_error(
                    INVALID_ADDRESS_OR_KEY,
                    zaino_source::GetSpentInfoError::NotSpent,
                )
            })
    }

    async fn get_address_tx_ids(&self, request: AddressTxIdsRequestWire) -> RpcResult<Vec<String>> {
        let request = request.into_domain().map_err(addresses_error)?;
        let txids = self
            .snapshot()
            .address_txids(&request.addresses, request.start, request.end)
            .await
            .map_err(service_error)?;
        Ok(txids
            .into_iter()
            .map(|txid| display_hex(txid.into()))
            .collect())
    }

    async fn get_address_utxos(
        &self,
        address_strings: AddressUtxosRequestWire,
    ) -> RpcResult<AddressUtxosWire> {
        let request = address_strings.into_domain().map_err(addresses_error)?;
        let start = u32::try_from(request.start_height)
            .ok()
            .and_then(|height| Height::try_from(height).ok());
        let max_entries = usize::try_from(request.max_entries)
            .map_err(|error| rpc_error(INVALID_PARAMETER, error))?;
        let page = UtxoPage::new(start.unwrap_or(Height::GENESIS), max_entries)
            .map_err(|error| rpc_error(INVALID_PARAMETER, error))?;
        let snapshot = self.snapshot();
        let (utxos, tip) = match start {
            Some(_) => snapshot
                .address_utxos(&request.addresses, page)
                .await
                .map_err(service_error)?,
            // as zebrad: a start past the highest height selects nothing
            None => (Vec::new(), snapshot.tip()),
        };
        Ok(AddressUtxosWire::from_domain(
            utxos,
            request.chain_info.then_some(tip),
        ))
    }

    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> RpcResult<GetAddressDeltasResponse> {
        if let GetAddressDeltasParams::Filtered { addresses, .. } = &params {
            if addresses.len() > MAX_REQUEST_ADDRESSES {
                return Err(addresses_error(AddressesError::TooMany));
            }
        }
        let request = params.into_domain().map_err(|error| match error {
            error @ GetAddressDeltasError::Address { .. } => {
                rpc_error(INVALID_ADDRESS_OR_KEY, error)
            }
        })?;
        self.snapshot()
            .address_deltas(request)
            .await
            .map(GetAddressDeltasResponse::from_domain)
            .map_err(service_error)
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> RpcResult<NetworkSolPsWire> {
        // as zebrad: a negative height is the tip
        let height = height
            .and_then(|height| u32::try_from(height).ok())
            .map(Height::try_from)
            .transpose()
            .map_err(|error| rpc_error(INVALID_PARAMETER, error))?;
        self.snapshot()
            .network_sol_ps(SolutionRateWindow::from_rpc(blocks), height)
            .await
            .map(NetworkSolPsWire)
            .map_err(service_error)
    }
}
