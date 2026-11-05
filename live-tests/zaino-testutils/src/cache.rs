//! The fixture cache: chains, wallets and an indexer database built once per
//! fixture and copied by every test.
//!
//! `cache/<name>-<key>/` holds `state/` (zebrad's state directory), `zaino/`
//! (zainod's finalised database at the same tip), `wallets/<party>.sqlite`,
//! and for the main fixture two competing heads: `head-a/state/` and
//! `head-b/blocks.hex`, the raw blocks of a branch one block longer that a
//! test feeds to a node with `submitblock`. `manifest.toml` records what the
//! fixture was built from and every transaction in it. `<key>` hashes the
//! build inputs, so a changed fixture, binary or database format rebuilds;
//! builds are serialised through a file lock and land by rename, so a
//! crashed build leaves no half fixture.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zaino_primitives::types::ActivationHeights;

use crate::indexer::{Backend, IndexerConfig, Zainod};
use crate::librustzcash::LrzWallet;
use crate::validator::{
    Pool, Validator, FILLER_ADDRESS, REGTEST_ACTIVATION_HEIGHTS, TRANSPARENT_MINER_ADDRESS,
};
use crate::wallet::{PoolBalances, Wallet as _};
use crate::zebrad::{Zebrad, ZebradConfig};

/// Bump when the builder's recipe changes without a fixture change.
const RECIPE: u32 = 5;
/// The indexer database format the cache holds; bump with the store.
const ZAINO_DB_FORMAT: &str = "zainodb-v1-1.3.0";
/// The main fixture's tip: past the real seam with a 1,200-block finalised band.
pub const MAIN_HEIGHT: u32 = 2_200;
/// How far the competing heads extend the main chain; zebra reorgs at most
/// 1,000 deep, so `head-b` at main + 1,000 reorgs a `head-a` node 999 deep.
pub const HEAD_A_EXTENSION: u32 = 999;
/// Blocks `head-b` adds above the main chain: one more than `head-a`.
pub const HEAD_B_EXTENSION: u32 = 1_000;
/// Each recorded send carries this much.
pub const SEND: u64 = 250_000;
const SYNC: Duration = Duration::from_secs(300);
const FINALISE: Duration = Duration::from_secs(300);
/// Orchard coinbase blocks the faucet starts with: enough notes for a
/// test's sends without waiting on change.
const FAUCET_NOTES: u32 = 20;
/// Transparent coinbase blocks the faucet holds, matured well before the tip.
const FAUCET_TRANSPARENT: u32 = 20;

/// What a fixture is built from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fixture {
    /// Directory prefix.
    pub name: String,
    /// The schedule.
    pub heights: ActivationHeights,
    /// Coinbase pool of the mined blocks.
    pub coinbase: Pool,
    /// Tip height.
    pub height: u32,
    /// Whether the competing heads and the recorded activity are built.
    pub full: bool,
}

impl Fixture {
    /// The main fixture: the canonical schedule, Orchard coinbase,
    /// [`MAIN_HEIGHT`] blocks, activity in every pool, competing heads.
    pub fn main() -> Self {
        Self {
            name: "main".into(),
            heights: REGTEST_ACTIVATION_HEIGHTS,
            coinbase: Pool::Orchard,
            height: MAIN_HEIGHT,
            full: true,
        }
    }

    /// A short pre-NU6.3 chain, NU6.3 and NU7 never activating, with the
    /// faucet funded: the Orchard-era compatibility fixture.
    pub fn orchard_only() -> Self {
        Self {
            name: "orchard_only".into(),
            heights: ActivationHeights {
                nu6_3: None,
                nu7: None,
                ..REGTEST_ACTIVATION_HEIGHTS
            },
            coinbase: Pool::Orchard,
            height: 8,
            full: false,
        }
    }

    /// A short chain on `heights` ending two blocks below `boundary`, with
    /// the faucet funded, for tests that cross an activation.
    pub fn boundary(name: &str, heights: ActivationHeights, boundary: u32) -> Self {
        Self {
            name: name.into(),
            heights,
            coinbase: Pool::Orchard,
            height: boundary.saturating_sub(2),
            full: false,
        }
    }
}

/// A wallet in the fixture.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Party {
    /// Receives every coinbase.
    Faucet,
    /// Receives the faucet's sends.
    Recipient,
}

impl Party {
    fn name(self) -> &'static str {
        match self {
            Party::Faucet => "faucet",
            Party::Recipient => "recipient",
        }
    }
}

/// A transaction the builder made.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// Height it was mined at.
    pub height: u32,
    /// Txid, display order hex.
    pub txid: String,
    /// `"send"` or `"shield"`.
    pub kind: String,
    /// Who built it.
    pub from: Party,
    /// Who received it (`shield`: the same party).
    pub to: Party,
    /// Pool the value landed in.
    pub pool: Pool,
    /// Value in zatoshis.
    pub amount: u64,
}

/// One party's addresses and balance at the tip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PartyState {
    /// Default transparent address.
    pub transparent: String,
    /// Default Sapling address.
    pub sapling: String,
    /// Default unified address.
    pub unified: String,
    /// Spendable balance per pool at the tip.
    pub balances: PoolBalances,
}

/// A competing head above the main chain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Head {
    /// Tip height.
    pub height: u32,
    /// Tip hash.
    pub tip_hash: String,
}

/// What the cached fixture was built from and what it contains.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Builder recipe version.
    pub recipe: u32,
    /// `zebrad --version`.
    pub zebrad: String,
    /// The indexer database format.
    pub zaino_db: String,
    /// The fixture.
    pub fixture: Fixture,
    /// Tip hash.
    pub tip_hash: String,
    /// The faucet at the tip.
    pub faucet: PartyState,
    /// The recipient at the tip.
    pub recipient: PartyState,
    /// Every transaction, by height.
    pub events: Vec<Event>,
    /// `head-a/state`, the main chain plus [`HEAD_A_EXTENSION`] blocks.
    pub head_a: Option<Head>,
    /// `head-b/blocks.hex`, a competing branch of [`HEAD_B_EXTENSION`] blocks.
    pub head_b: Option<Head>,
}

impl Manifest {
    fn key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.recipe.to_le_bytes());
        hasher.update(self.zebrad.as_bytes());
        hasher.update(self.zaino_db.as_bytes());
        hasher.update(format!("{:?}", self.fixture).as_bytes());
        hex::encode(&hasher.finalize()[..12])
    }

    /// The events mined at `height`.
    pub fn events_at(&self, height: u32) -> impl Iterator<Item = &Event> {
        self.events
            .iter()
            .filter(move |event| event.height == height)
    }
}

/// A built fixture.
#[derive(Clone, Debug)]
pub struct ChainCache {
    dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
}

impl ChainCache {
    /// `ZAINO_CHAIN_CACHE`, else `live-tests/.cache`.
    pub fn root() -> PathBuf {
        std::env::var_os("ZAINO_CHAIN_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join(".cache")
            })
    }

    /// The main fixture, built on first use.
    pub async fn ensure() -> Result<Self> {
        Self::ensure_fixture(Fixture::main()).await
    }

    /// `fixture`, built if its key is not on disk.
    pub async fn ensure_fixture(fixture: Fixture) -> Result<Self> {
        let zebrad = crate::zebrad::version(&Zebrad::binary()).await?;
        let manifest = Manifest {
            recipe: RECIPE,
            zebrad,
            zaino_db: ZAINO_DB_FORMAT.into(),
            fixture: fixture.clone(),
            tip_hash: String::new(),
            faucet: PartyState::default(),
            recipient: PartyState::default(),
            events: Vec::new(),
            head_a: None,
            head_b: None,
        };
        let root = Self::root();
        std::fs::create_dir_all(&root)?;
        let dir = root.join(format!("{}-{}", fixture.name, manifest.key()));

        let lock_path = root.join(format!("{}.lock", fixture.name));
        let lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let file = std::fs::File::create(lock_path)?;
            file.lock()?;
            Ok(file)
        })
        .await??;

        let manifest_path = dir.join("manifest.toml");
        let built = if manifest_path.exists() {
            toml::from_str(&std::fs::read_to_string(&manifest_path)?)?
        } else {
            let building = root.join(format!("{}-{}.building", fixture.name, manifest.key()));
            if building.exists() {
                std::fs::remove_dir_all(&building)?;
            }
            std::fs::create_dir_all(&building)?;
            let built = build(manifest, &building).await.with_context(|| {
                format!("build fixture {} in {}", fixture.name, building.display())
            })?;
            std::fs::write(building.join("manifest.toml"), toml::to_string(&built)?)?;
            std::fs::rename(&building, &dir)?;
            built
        };
        drop(lock);
        Ok(Self {
            dir,
            manifest: built,
        })
    }

    /// The main chain's zebrad state directory.
    pub fn state_dir(&self) -> PathBuf {
        self.dir.join("state")
    }

    /// zainod's finalised database at the main tip.
    pub fn zaino_db_dir(&self) -> PathBuf {
        self.dir.join("zaino")
    }

    /// `party`'s wallet database at the main tip.
    pub fn wallet_db(&self, party: Party) -> PathBuf {
        self.dir
            .join("wallets")
            .join(format!("{}.sqlite", party.name()))
    }

    /// `head-a`'s zebrad state directory.
    pub fn head_a_state_dir(&self) -> PathBuf {
        self.dir.join("head-a").join("state")
    }

    /// `head-b`'s blocks above the main tip, raw hex in height order.
    pub fn head_b_blocks(&self) -> Result<Vec<String>> {
        let path = self.dir.join("head-b").join("blocks.hex");
        Ok(std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// A zebrad launch from the main chain.
    pub fn zebrad_config(&self) -> ZebradConfig {
        ZebradConfig::regtest(self.manifest.fixture.heights)
            .mine_to(self.manifest.fixture.coinbase)
            .state_from(self.state_dir())
            .expect_height(self.manifest.fixture.height)
    }

    /// A zebrad launch from `head-a`.
    pub fn head_a_config(&self) -> ZebradConfig {
        ZebradConfig::regtest(self.manifest.fixture.heights)
            .mine_to(self.manifest.fixture.coinbase)
            .state_from(self.head_a_state_dir())
            .expect_height(self.manifest.fixture.height + HEAD_A_EXTENSION)
    }

    /// An indexer over `zebrad` starting from the cached database.
    pub fn indexer_config(&self, zebrad: &impl Validator, backend: Backend) -> IndexerConfig {
        IndexerConfig::new(zebrad, backend).db_from(self.zaino_db_dir())
    }

    /// `party`'s wallet, restored over its cached database and synced.
    pub async fn wallet(
        &self,
        party: Party,
        zebrad: &impl Validator,
        indexer: &Zainod,
    ) -> Result<LrzWallet> {
        let mnemonic = match party {
            Party::Faucet => crate::validator::FAUCET_SEED,
            Party::Recipient => crate::validator::RECIPIENT_SEED,
        };
        let wallet = LrzWallet::restore_from_db(
            party.name(),
            mnemonic,
            zebrad,
            indexer,
            &self.wallet_db(party),
        )
        .await?;
        wallet.sync().await?;
        Ok(wallet)
    }
}

/// Mines the fixture and writes its artefacts into `dir`.
async fn build(mut manifest: Manifest, dir: &Path) -> Result<Manifest> {
    let fixture = manifest.fixture.clone();
    let zebrad =
        Zebrad::launch(ZebradConfig::regtest(fixture.heights).mine_to(fixture.coinbase)).await?;
    let zaino = Zainod::launch(IndexerConfig::new(&zebrad, Backend::Fetch)).await?;
    let mut events = Vec::new();

    // A boundary fixture ends two below its boundary: the faucet's coinbase
    // notes, then one block confirming a transparent receipt for the
    // recipient, so a shield of it can still target a pre-boundary block.
    let notes = if fixture.full {
        FAUCET_NOTES
    } else {
        fixture.height.saturating_sub(2).max(1)
    };
    let faucet = LrzWallet::funded_faucet(&zebrad, &zaino, fixture.coinbase, notes).await?;
    let recipient = LrzWallet::recipient(&zebrad, &zaino).await?;
    if !fixture.full {
        let to = recipient.address(Pool::Transparent).await?;
        let txid = faucet.send(&to, SEND).await?;
        // Confirmed by the configured miner, so every block of a short
        // fixture carries the coinbase its tests reason about.
        let height = zebrad.generate_blocks(1).await?;
        zaino.wait_for_block(height, SYNC).await?;
        events.push(Event {
            height,
            txid: txid.to_string(),
            kind: "send".into(),
            from: Party::Faucet,
            to: Party::Recipient,
            pool: Pool::Transparent,
            amount: SEND,
        });
        faucet.sync().await?;
        recipient.sync().await?;
    }
    if fixture.full {
        let tip = zebrad
            .generate_blocks_to(FAUCET_TRANSPARENT, TRANSPARENT_MINER_ADDRESS)
            .await?;
        zaino.wait_for_block(tip, SYNC).await?;
        faucet.sync().await?;

        // Activity deep in the finalised band, straddling the seam, and at
        // the top: every pool each time, plus a shield.
        let seam = fixture.height.saturating_sub(zaino_consensus_seam());
        for stop in [tip + 5, seam.saturating_sub(10), fixture.height - 50] {
            fill_to(&zebrad, &zaino, stop).await?;
            activity(&zebrad, &zaino, &faucet, &recipient, &mut events).await?;
        }
    }
    let tip = fill_to(&zebrad, &zaino, fixture.height).await?;
    anyhow::ensure!(
        tip == fixture.height,
        "mined to {tip}, wanted {}",
        fixture.height
    );
    faucet.sync().await?;
    recipient.sync().await?;
    if tip > zaino_consensus_seam() {
        crate::finalised::wait_for_finalised(&zaino, tip - zaino_consensus_seam(), FINALISE)
            .await?;
    }

    manifest.tip_hash = zebrad.best_block_hash().await?;
    manifest.faucet = party_state(&faucet).await?;
    manifest.recipient = party_state(&recipient).await?;
    manifest.events = events;

    std::fs::create_dir_all(dir.join("wallets"))?;
    std::fs::copy(faucet.db_path(), dir.join("wallets").join("faucet.sqlite"))?;
    std::fs::copy(
        recipient.db_path(),
        dir.join("wallets").join("recipient.sqlite"),
    )?;
    drop(faucet);
    drop(recipient);
    crate::process::copy_dir(&zaino.db_dir(), &dir.join("zaino"))?;
    zaino.shutdown();
    zebrad.snapshot(&dir.join("state")).await?;

    if fixture.full {
        let a = Zebrad::launch(
            ZebradConfig::regtest(fixture.heights)
                .mine_to(fixture.coinbase)
                .state_from(dir.join("state"))
                .expect_height(fixture.height),
        )
        .await?;
        let height = a
            .generate_blocks_to(HEAD_A_EXTENSION, FILLER_ADDRESS)
            .await?;
        manifest.head_a = Some(Head {
            height,
            tip_hash: a.best_block_hash().await?,
        });
        a.snapshot(&dir.join("head-a").join("state")).await?;

        let b = Zebrad::launch(
            ZebradConfig::regtest(fixture.heights)
                .mine_to(fixture.coinbase)
                .state_from(dir.join("state"))
                .expect_height(fixture.height),
        )
        .await?;
        let height = b
            .generate_blocks_to(HEAD_B_EXTENSION, FILLER_ADDRESS)
            .await?;
        let mut blocks = String::new();
        for h in (fixture.height + 1)..=height {
            blocks.push_str(&b.raw_block(h).await?);
            blocks.push('\n');
        }
        std::fs::create_dir_all(dir.join("head-b"))?;
        std::fs::write(dir.join("head-b").join("blocks.hex"), blocks)?;
        manifest.head_b = Some(Head {
            height,
            tip_hash: b.best_block_hash().await?,
        });
    }
    Ok(manifest)
}

/// Mines filler up to `height` and waits for the indexer.
async fn fill_to(zebrad: &Zebrad, zaino: &Zainod, height: u32) -> Result<u32> {
    let tip = zebrad.chain_height().await?;
    if height <= tip {
        return Ok(tip);
    }
    let tip = zebrad
        .generate_blocks_to(height - tip, FILLER_ADDRESS)
        .await?;
    zaino.wait_for_block(tip, SYNC).await?;
    Ok(tip)
}

/// One send into each pool, a shield of the recipient's transparent funds,
/// and a send back, each confirmed in its own block and recorded.
async fn activity(
    zebrad: &Zebrad,
    zaino: &Zainod,
    faucet: &LrzWallet,
    recipient: &LrzWallet,
    events: &mut Vec<Event>,
) -> Result<()> {
    faucet.sync().await?;
    recipient.sync().await?;
    for pool in [Pool::Transparent, Pool::Sapling, Pool::Orchard] {
        let to = recipient.address(pool).await?;
        let txid = faucet.send(&to, SEND).await?;
        let height = confirm(zebrad, zaino).await?;
        events.push(Event {
            height,
            txid: txid.to_string(),
            kind: "send".into(),
            from: Party::Faucet,
            to: Party::Recipient,
            pool: received_pool(zebrad, pool, height).await?,
            amount: SEND,
        });
        faucet.sync().await?;
    }
    recipient.sync().await?;
    let txid = recipient.shield().await?;
    let height = confirm(zebrad, zaino).await?;
    events.push(Event {
        height,
        txid: txid.to_string(),
        kind: "shield".into(),
        from: Party::Recipient,
        to: Party::Recipient,
        pool: received_pool(zebrad, Pool::Orchard, height).await?,
        amount: SEND,
    });
    recipient.sync().await?;
    let to = faucet.address(Pool::Orchard).await?;
    let txid = recipient.send(&to, SEND / 2).await?;
    let height = confirm(zebrad, zaino).await?;
    events.push(Event {
        height,
        txid: txid.to_string(),
        kind: "send".into(),
        from: Party::Recipient,
        to: Party::Faucet,
        pool: received_pool(zebrad, Pool::Orchard, height).await?,
        amount: SEND / 2,
    });
    faucet.sync().await?;
    recipient.sync().await?;
    Ok(())
}

/// Mines one block and waits for the indexer; the height it was mined at.
async fn confirm(zebrad: &Zebrad, zaino: &Zainod) -> Result<u32> {
    let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
    zaino.wait_for_block(tip, SYNC).await?;
    Ok(tip)
}

/// The pool a unified-address receipt lands in at `height`: Ironwood once
/// NU6.3 is active.
async fn received_pool(zebrad: &Zebrad, sent_to: Pool, height: u32) -> Result<Pool> {
    if sent_to != Pool::Orchard {
        return Ok(sent_to);
    }
    let heights = zebrad.activation_heights().await?;
    Ok(match heights.nu6_3 {
        Some(nu6_3) if height >= nu6_3 => Pool::Ironwood,
        _ => Pool::Orchard,
    })
}

async fn party_state(wallet: &LrzWallet) -> Result<PartyState> {
    Ok(PartyState {
        transparent: wallet.address(Pool::Transparent).await?,
        sapling: wallet.address(Pool::Sapling).await?,
        unified: wallet.address(Pool::Orchard).await?,
        balances: wallet.balances().await?,
    })
}

/// The non-finalised depth the indexer runs at.
fn zaino_consensus_seam() -> u32 {
    zaino_consensus::MAX_NONFINALISED_DEPTH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boundary_fixture_stops_two_below_the_boundary() {
        let fixture = Fixture::boundary("nu7", REGTEST_ACTIVATION_HEIGHTS, 6);
        assert_eq!(fixture.height, 4);
        assert!(!fixture.full);
    }

    #[test]
    fn the_key_changes_with_every_input() {
        let base = Manifest {
            recipe: RECIPE,
            zebrad: "zebrad 7.0.0".into(),
            zaino_db: ZAINO_DB_FORMAT.into(),
            fixture: Fixture::main(),
            tip_hash: String::new(),
            faucet: PartyState::default(),
            recipient: PartyState::default(),
            events: Vec::new(),
            head_a: None,
            head_b: None,
        };
        let mut other_zebrad = base.clone();
        other_zebrad.zebrad = "zebrad 7.1.0".into();
        let mut other_db = base.clone();
        other_db.zaino_db = "other".into();
        let mut other_fixture = base.clone();
        other_fixture.fixture.height += 1;
        for other in [other_zebrad, other_db, other_fixture] {
            assert_ne!(base.key(), other.key());
        }
        let mut same = base.clone();
        same.tip_hash = "differs but is an output".into();
        assert_eq!(base.key(), same.key());
    }
}
