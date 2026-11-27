//! In-process librustzcash wallet: the [`Wallet`] implementation.
//!
//! A `zcash_client_sqlite` wallet scanned through the indexer under test
//! with `zcash_client_backend::sync`, signing with the bundled prover. A
//! regtest wallet takes its schedule from the running validator, so wallet
//! and validator never disagree on heights.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use secrecy::{ExposeSecret as _, SecretVec};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use zcash_client_backend::data_api::chain::{error::Error as ChainError, BlockCache, BlockSource};
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::data_api::wallet::input_selection::{GreedyInputSelector, SpendPolicy};
use zcash_client_backend::data_api::wallet::{
    create_proposed_transactions, propose_transfer, shield_transparent_funds, ConfirmationsPolicy,
    SpendingKeys,
};
use zcash_client_backend::data_api::{AccountBirthday, WalletRead, WalletSummary, WalletWrite};
use zcash_client_backend::fees::standard::SingleOutputChangeStrategy;
use zcash_client_backend::fees::{DustOutputPolicy, StandardFeeRule};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zcash_client_backend::proto::service::{BlockId, RawTransaction};
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_backend::zip321::{Payment, TransactionRequest};
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_keys::address::Address;
use zcash_keys::encoding::AddressCodec as _;
use zcash_keys::keys::{ReceiverRequirement, UnifiedAddressRequest, UnifiedSpendingKey};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters as _};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{ShieldedPool, TxId};

use crate::indexer::Zainod;
use crate::validator::{Network, Pool, Validator, FAUCET_SEED, FILLER_ADDRESS, RECIPIENT_SEED};
use crate::wallet::{PoolBalances, Wallet};

/// Blocks a transparent coinbase needs before it can be spent.
pub const COINBASE_MATURITY: u32 = 100;
const SYNC_BATCH_SIZE: u32 = 100;
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);
const LABEL: &str = "zaino-testutils";

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

/// A librustzcash wallet with one account.
pub struct LrzWallet {
    name: &'static str,
    db: AsyncMutex<Db>,
    db_path: PathBuf,
    usk: UnifiedSpendingKey,
    account: AccountUuid,
    params: LocalNetwork,
    indexer_uri: String,
    _dir: TempDir,
}

impl std::fmt::Debug for LrzWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LrzWallet")
            .field("name", &self.name)
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

impl LrzWallet {
    /// Restores `mnemonic` as a fresh wallet with birthday 1, scanning
    /// through `indexer`.
    pub async fn restore(
        name: &'static str,
        mnemonic: &str,
        validator: &impl Validator,
        indexer: &Zainod,
    ) -> Result<Self> {
        Self::open(name, mnemonic, validator, indexer, None).await
    }

    /// Restores `mnemonic` over a copy of the wallet database at `db`, so the
    /// scan resumes where that database left off.
    pub async fn restore_from_db(
        name: &'static str,
        mnemonic: &str,
        validator: &impl Validator,
        indexer: &Zainod,
        db: &Path,
    ) -> Result<Self> {
        Self::open(name, mnemonic, validator, indexer, Some(db)).await
    }

    async fn open(
        name: &'static str,
        mnemonic: &str,
        validator: &impl Validator,
        indexer: &Zainod,
        cached: Option<&Path>,
    ) -> Result<Self> {
        let params = local_network(validator).await?;
        let mnemonic = bip0039::Mnemonic::<bip0039::English>::from_phrase(mnemonic)
            .map_err(|e| anyhow!("{name}: invalid mnemonic: {e}"))?;
        let seed = SecretVec::new(mnemonic.to_seed("").to_vec());

        let dir = tempfile::Builder::new()
            .prefix(&format!("zaino-wallet-{name}-"))
            .tempdir()?;
        let db_path = dir.path().join("wallet.sqlite");
        if let Some(cached) = cached {
            std::fs::copy(cached, &db_path)
                .with_context(|| format!("{name}: copy cached wallet {}", cached.display()))?;
        }
        let mut db = WalletDb::for_path(&db_path, params, SystemClock, UnwrapErr(SysRng))
            .map_err(|e| anyhow!("{name}: open wallet db: {e}"))?;
        init_wallet_db(&mut db, None).map_err(|e| anyhow!("{name}: init wallet db: {e}"))?;
        let indexer_uri = indexer.grpc_uri();

        let (account, usk) = match cached {
            // The cached database holds the account; derive its key again.
            Some(_) => {
                let account = db
                    .get_account_ids()
                    .map_err(|e| anyhow!("{name}: list accounts: {e}"))?
                    .into_iter()
                    .next()
                    .with_context(|| format!("{name}: cached wallet has no account"))?;
                let usk = UnifiedSpendingKey::from_seed(
                    &params,
                    seed.expose_secret(),
                    zip32::AccountId::ZERO,
                )
                .map_err(|e| anyhow!("{name}: derive spending key: {e:?}"))?;
                (account, usk)
            }
            None => {
                let mut client = connect(&indexer_uri).await?;
                let treestate = client
                    .get_tree_state(BlockId {
                        height: 1,
                        hash: vec![],
                    })
                    .await
                    .map_err(|e| anyhow!("{name}: get_tree_state(1): {e}"))?
                    .into_inner();
                let birthday = AccountBirthday::from_treestate(treestate, None)
                    .map_err(|_| anyhow!("{name}: invalid birthday treestate"))?;
                db.create_account(LABEL, &seed, &birthday, None)
                    .map_err(|e| anyhow!("{name}: create account: {e}"))?
            }
        };

        Ok(Self {
            name,
            db: AsyncMutex::new(db),
            db_path,
            usk,
            account,
            params,
            indexer_uri,
            _dir: dir,
        })
    }

    /// The unfunded faucet (`FAUCET_SEED`), owner of every miner address.
    pub async fn faucet(validator: &impl Validator, indexer: &Zainod) -> Result<Self> {
        Self::restore("faucet", FAUCET_SEED, validator, indexer).await
    }

    /// The recipient (`RECIPIENT_SEED`).
    pub async fn recipient(validator: &impl Validator, indexer: &Zainod) -> Result<Self> {
        Self::restore("recipient", RECIPIENT_SEED, validator, indexer).await
    }

    /// The faucet holding `notes` spendable coinbase notes in `pool`, synced.
    ///
    /// A shielded coinbase is spendable once mined, so one block per note. A
    /// transparent coinbase is matured by [`COINBASE_MATURITY`] filler blocks
    /// and shielded, note by note.
    pub async fn funded_faucet(
        validator: &impl Validator,
        indexer: &Zainod,
        pool: Pool,
        notes: u32,
    ) -> Result<Self> {
        let faucet = Self::faucet(validator, indexer).await?;
        match pool {
            Pool::Transparent => {
                for _ in 0..notes {
                    let tip = validator.generate_blocks(1).await?;
                    indexer.wait_for_block(tip, SYNC_TIMEOUT).await?;
                    let tip = validator
                        .generate_blocks_to(COINBASE_MATURITY, FILLER_ADDRESS)
                        .await?;
                    indexer.wait_for_block(tip, SYNC_TIMEOUT).await?;
                    faucet.sync().await?;
                    faucet.shield().await?;
                }
                let tip = validator.generate_blocks_to(1, FILLER_ADDRESS).await?;
                indexer.wait_for_block(tip, SYNC_TIMEOUT).await?;
            }
            Pool::Sapling | Pool::Orchard | Pool::Ironwood => {
                let tip = validator.generate_blocks(notes).await?;
                indexer.wait_for_block(tip, SYNC_TIMEOUT).await?;
            }
        }
        faucet.sync().await?;
        Ok(faucet)
    }

    /// The wallet database file, for snapshotting.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Sends `zats` to `to` spending only notes in `pools`; an empty list
    /// means any shielded pool.
    pub async fn send_from(&self, pools: &[Pool], to: &str, zats: u64) -> Result<TxId> {
        let to_addr = Address::decode(&self.params, to)
            .with_context(|| format!("{}: bad recipient address {to:?}", self.name))?;
        let amount = Zatoshis::from_u64(zats)
            .map_err(|e| anyhow!("{}: bad amount {zats}: {e:?}", self.name))?;
        let spend_policy = spend_policy(pools)?;
        let prover = LocalTxProver::bundled();
        let input_selector = GreedyInputSelector::<Db>::new();
        let keys = SpendingKeys::from_unified_spending_key(self.usk.clone());

        let mut db = self.db.lock().await;
        let target = target_height(&db, self.name)?;
        let change_strategy = SingleOutputChangeStrategy::<Db>::new(
            StandardFeeRule::Zip317,
            None,
            change_pool(&self.params, target),
            DustOutputPolicy::default(),
        );
        let request = TransactionRequest::new(vec![Payment::new(
            to_addr.to_zcash_address(&self.params),
            Some(amount),
            None,
            None,
            None,
            vec![],
        )
        .map_err(|e| anyhow!("{}: build payment: {e:?}", self.name))?])
        .map_err(|e| anyhow!("{}: build request: {e:?}", self.name))?;
        let proposal = propose_transfer::<Db, LocalNetwork, _, _, std::convert::Infallible>(
            &mut *db,
            &self.params,
            self.account,
            &input_selector,
            &change_strategy,
            request,
            one_confirmation(),
            &spend_policy,
            None,
            None,
        )
        .map_err(|e| anyhow!("{}: propose transfer: {e}", self.name))?;
        let txids = create_proposed_transactions::<
            Db,
            LocalNetwork,
            std::convert::Infallible,
            _,
            std::convert::Infallible,
            _,
        >(
            &mut *db,
            &self.params,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            &prover,
            &prover,
            &keys,
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| anyhow!("{}: create transactions: {e}", self.name))?;
        let txids: Vec<TxId> = txids.into_iter().collect();
        let raw = raw_transactions(&db, &txids, self.name)?;
        drop(db);
        broadcast(&self.indexer_uri, raw, self.name).await?;
        single(txids, self.name)
    }
}

impl Wallet for LrzWallet {
    async fn address(&self, pool: Pool) -> Result<String> {
        use ReceiverRequirement::{Allow, Require};
        // The default address (diversifier index 0), the one coinbase pays.
        let request = match pool {
            Pool::Orchard | Pool::Ironwood => UnifiedAddressRequest::custom(Require, Allow, Allow),
            Pool::Sapling => UnifiedAddressRequest::custom(Allow, Require, Allow),
            Pool::Transparent => UnifiedAddressRequest::custom(Allow, Allow, Require),
        }
        .map_err(|e| anyhow!("{}: address request: {e}", self.name))?;
        let (ua, _) = self
            .usk
            .to_unified_full_viewing_key()
            .default_address(request)
            .map_err(|e| anyhow!("{}: default address: {e:?}", self.name))?;
        Ok(match pool {
            Pool::Orchard | Pool::Ironwood => ua.encode(&self.params),
            Pool::Sapling => ua
                .sapling()
                .map(|s| s.encode(&self.params))
                .with_context(|| format!("{}: no sapling receiver", self.name))?,
            Pool::Transparent => ua
                .transparent()
                .map(|t| t.encode(&self.params))
                .with_context(|| format!("{}: no transparent receiver", self.name))?,
        })
    }

    async fn sync(&self) -> Result<()> {
        let mut client = connect(&self.indexer_uri).await?;
        let cache = MemBlockCache::default();
        let mut db = self.db.lock().await;
        zcash_client_backend::sync::run(
            &mut client,
            &self.params,
            &cache,
            &mut *db,
            SYNC_BATCH_SIZE,
        )
        .await
        .map_err(|e| anyhow!("{}: sync: {e}", self.name))
    }

    async fn balances(&self) -> Result<PoolBalances> {
        let db = self.db.lock().await;
        let summary = db
            .get_wallet_summary(one_confirmation())
            .map_err(|e| anyhow!("{}: wallet summary: {e}", self.name))?;
        Ok(pool_balances(summary.as_ref(), self.account))
    }

    async fn send(&self, to: &str, zats: u64) -> Result<TxId> {
        self.send_from(&[], to, zats).await
    }

    async fn shield(&self) -> Result<TxId> {
        let prover = LocalTxProver::bundled();
        let input_selector = GreedyInputSelector::<Db>::new();
        let keys = SpendingKeys::from_unified_spending_key(self.usk.clone());
        let mut db = self.db.lock().await;
        let target = target_height(&db, self.name)?;
        let change_strategy = SingleOutputChangeStrategy::<Db>::new(
            StandardFeeRule::Zip317,
            None,
            change_pool(&self.params, target),
            DustOutputPolicy::default(),
        );
        let from: Vec<_> = db
            .get_transparent_receivers(self.account, true, true)
            .map_err(|e| anyhow!("{}: transparent receivers: {e}", self.name))?
            .into_keys()
            .collect();
        let txids = shield_transparent_funds::<Db, LocalNetwork, _, _>(
            &mut *db,
            &self.params,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            &prover,
            &prover,
            &input_selector,
            &change_strategy,
            Zatoshis::ZERO,
            &keys,
            &from,
            self.account,
            one_confirmation(),
        )
        .map_err(|e| anyhow!("{}: shield: {e}", self.name))?;
        let txids: Vec<TxId> = txids.into_iter().collect();
        let raw = raw_transactions(&db, &txids, self.name)?;
        drop(db);
        broadcast(&self.indexer_uri, raw, self.name).await?;
        single(txids, self.name)
    }
}

/// The validator's schedule as librustzcash's regtest parameters.
async fn local_network(validator: &impl Validator) -> Result<LocalNetwork> {
    match validator.network() {
        Network::Regtest(_) => {}
        Network::Testnet | Network::Mainnet => {
            bail!("the in-process wallet runs on regtest only")
        }
    }
    let heights = validator.activation_heights().await?;
    let at = |height: Option<u32>| height.map(BlockHeight::from_u32);
    Ok(LocalNetwork {
        overwinter: at(heights.overwinter),
        sapling: at(heights.sapling),
        blossom: at(heights.blossom),
        heartwood: at(heights.heartwood),
        canopy: at(heights.canopy),
        nu5: at(heights.nu5),
        nu6: at(heights.nu6),
        nu6_1: at(heights.nu6_1),
        nu6_2: at(heights.nu6_2),
        nu6_3: at(heights.nu6_3),
        nu7: at(heights.nu7),
    })
}

/// Empty `pools` is the default policy; otherwise spends only those pools.
fn spend_policy(pools: &[Pool]) -> Result<SpendPolicy> {
    if pools.is_empty() {
        return Ok(SpendPolicy::default());
    }
    let shielded = pools
        .iter()
        .map(|pool| match pool {
            Pool::Sapling => Ok(ShieldedPool::Sapling),
            Pool::Orchard => Ok(ShieldedPool::Orchard),
            Pool::Ironwood => Ok(ShieldedPool::Ironwood),
            Pool::Transparent => Err(anyhow!("transparent is not a shielded source")),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SpendPolicy::shielded_pools(shielded))
}

/// Change goes to Ironwood once NU6.3 is active at the target height,
/// where Orchard is spend-locked; Orchard before.
fn change_pool(params: &LocalNetwork, target: BlockHeight) -> ShieldedPool {
    if params.is_nu_active(NetworkUpgrade::Nu6_3, target) {
        ShieldedPool::Ironwood
    } else {
        ShieldedPool::Orchard
    }
}

/// The height a transaction built now targets: the synced tip plus one.
fn target_height(db: &Db, name: &str) -> Result<BlockHeight> {
    let tip = db
        .chain_height()
        .map_err(|e| anyhow!("{name}: chain height: {e}"))?
        .with_context(|| format!("{name}: wallet has no synced tip"))?;
    Ok(tip + 1)
}

fn one_confirmation() -> ConfirmationsPolicy {
    ConfirmationsPolicy::new_symmetrical(NonZeroU32::MIN, false)
}

async fn connect(uri: &str) -> Result<CompactTxStreamerClient<tonic::transport::Channel>> {
    let channel = tonic::transport::Channel::from_shared(uri.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .with_context(|| format!("connect to {uri}"))?;
    Ok(CompactTxStreamerClient::new(channel))
}

/// Built transactions as consensus bytes, read under the db lock.
fn raw_transactions(db: &Db, txids: &[TxId], name: &str) -> Result<Vec<Vec<u8>>> {
    txids
        .iter()
        .map(|txid| {
            let tx = db
                .get_transaction(*txid)
                .map_err(|e| anyhow!("{name}: get transaction {txid}: {e}"))?
                .with_context(|| format!("{name}: built transaction {txid} missing"))?;
            let mut data = Vec::new();
            tx.write(&mut data)?;
            Ok(data)
        })
        .collect()
}

/// Relays through the indexer's `SendTransaction`; building only stores locally.
async fn broadcast(uri: &str, raw: Vec<Vec<u8>>, name: &str) -> Result<()> {
    let relay = async {
        let mut client = connect(uri).await?;
        for data in raw {
            let reply = client
                .send_transaction(RawTransaction { data, height: 0 })
                .await
                .map_err(|e| anyhow!("{name}: send_transaction: {e}"))?
                .into_inner();
            if reply.error_code != 0 {
                bail!(
                    "{name}: indexer rejected the transaction: {} {}",
                    reply.error_code,
                    reply.error_message
                );
            }
        }
        Ok(())
    };
    tokio::time::timeout(RELAY_TIMEOUT, relay)
        .await
        .map_err(|_| anyhow!("{name}: relay timed out after {RELAY_TIMEOUT:?}"))?
}

fn single(txids: Vec<TxId>, name: &str) -> Result<TxId> {
    match txids.as_slice() {
        [txid] => Ok(*txid),
        other => bail!("{name}: expected one transaction, built {}", other.len()),
    }
}

fn pool_balances(
    summary: Option<&WalletSummary<AccountUuid>>,
    account: AccountUuid,
) -> PoolBalances {
    let Some(balance) = summary.and_then(|s| s.account_balances().get(&account)) else {
        return PoolBalances::default();
    };
    PoolBalances {
        transparent: u64::from(balance.unshielded_balance().spendable_value()),
        sapling: u64::from(balance.sapling_balance().spendable_value()),
        orchard: u64::from(balance.orchard_balance().spendable_value()),
        ironwood: u64::from(balance.ironwood_balance().spendable_value()),
    }
}

/// The compact blocks of one sync, in memory; regtest chains are short.
#[derive(Default)]
struct MemBlockCache {
    blocks: StdMutex<BTreeMap<u64, CompactBlock>>,
}

impl BlockSource for MemBlockCache {
    type Error = std::convert::Infallible;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let from = from_height.map(u64::from).unwrap_or(0);
        let blocks = self.blocks.lock().expect("block cache mutex poisoned");
        for (_, block) in blocks.range(from..).take(limit.unwrap_or(usize::MAX)) {
            with_block(block.clone())?;
        }
        Ok(())
    }
}

#[async_trait]
impl BlockCache for MemBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let blocks = self.blocks.lock().expect("block cache mutex poisoned");
        let tip = match range {
            None => blocks.keys().next_back().copied(),
            Some(range) => {
                let end = u64::from(range.block_range().end);
                blocks.range(..end).next_back().map(|(k, _)| *k)
            }
        };
        Ok(tip
            .and_then(|k| u32::try_from(k).ok())
            .map(BlockHeight::from_u32))
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let start = u64::from(range.block_range().start);
        let end = u64::from(range.block_range().end);
        let blocks = self.blocks.lock().expect("block cache mutex poisoned");
        Ok(blocks.range(start..end).map(|(_, b)| b.clone()).collect())
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut blocks = self.blocks.lock().expect("block cache mutex poisoned");
        for block in compact_blocks {
            blocks.insert(block.height, block);
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let start = u64::from(range.block_range().start);
        let end = u64::from(range.block_range().end);
        let mut blocks = self.blocks.lock().expect("block cache mutex poisoned");
        let keys: Vec<u64> = blocks.range(start..end).map(|(k, _)| *k).collect();
        for key in keys {
            blocks.remove(&key);
        }
        Ok(())
    }
}
