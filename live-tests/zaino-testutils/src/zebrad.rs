//! `zebrad` subprocess: the [`Validator`] implementation.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tempfile::TempDir;
use zaino_primitives::types::ActivationHeights;

use crate::process::{self, Process};
use crate::rpc::JsonRpcClient;
use crate::validator::{activation_heights_from_info, Network, Pool, Validator, LOCKBOX_ADDRESS};

/// Oldest zebrad the harness drives: the first release knowing the `NU7` key.
pub const ZEBRAD_MIN_VERSION: (u16, u16, u16) = (7, 0, 0);

/// `zebrad --version` output of the binary in use.
pub static ZEBRAD_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

const RPC_OPENED: &str = "zebra_rpc::server: Opened RPC endpoint at ";
const INDEXER_OPENED: &str = "zebra_rpc::indexer::server: Opened RPC endpoint at ";
const FATAL: &[&str] = &[" panicked at", "Address already in use"];
const START_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_GRACE: Duration = Duration::from_secs(60);

/// How to launch a zebrad.
#[derive(Clone, Debug)]
pub struct ZebradConfig {
    /// Network and, on regtest, the schedule.
    pub network: Network,
    /// Coinbase pool on regtest.
    pub coinbase: Pool,
    /// A state directory to start from: copied into the run directory on
    /// regtest, opened in place on the public networks (default:
    /// `ZEBRA_STATE_DIR`, else `~/.cache/zebra`).
    pub state_from: Option<PathBuf>,
    /// Fixed `(rpc, indexer gRPC)` ports instead of OS-assigned ones, so a
    /// respawned node keeps the addresses an indexer already holds.
    pub ports: Option<(u16, u16)>,
    /// The height a restored state holds; launch waits until the node
    /// reports it, since zebra loads its non-finalised state after its RPC
    /// comes up.
    pub expect_height: Option<u32>,
}

impl ZebradConfig {
    /// A fresh regtest on `heights`, coinbase to the transparent faucet address.
    pub fn regtest(heights: ActivationHeights) -> Self {
        Self {
            network: Network::Regtest(heights),
            coinbase: Pool::Transparent,
            state_from: None,
            ports: None,
            expect_height: None,
        }
    }

    /// Testnet or mainnet on an existing state directory; no mining.
    pub fn public(network: Network) -> Self {
        Self {
            network,
            coinbase: Pool::Transparent,
            state_from: None,
            ports: None,
            expect_height: None,
        }
    }

    /// Pays coinbase into `pool`.
    pub fn mine_to(mut self, pool: Pool) -> Self {
        self.coinbase = pool;
        self
    }

    /// Starts from the state directory at `dir`.
    pub fn state_from(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_from = Some(dir.into());
        self
    }

    /// Binds the RPC and indexer gRPC listeners to these ports.
    pub fn ports(mut self, rpc: u16, indexer: u16) -> Self {
        self.ports = Some((rpc, indexer));
        self
    }

    /// Waits at launch until the node reports `height`.
    pub fn expect_height(mut self, height: u32) -> Self {
        self.expect_height = Some(height);
        self
    }
}

/// A running zebrad.
#[derive(Debug)]
pub struct Zebrad {
    process: Process,
    rpc_addr: SocketAddr,
    indexer_grpc_addr: SocketAddr,
    state_dir: PathBuf,
    rpc: JsonRpcClient,
    network: Network,
    coinbase: Pool,
    _run_dir: TempDir,
}

impl Zebrad {
    /// The `zebrad` binary: `ZEBRAD_BIN`, else `zebrad` on `PATH`.
    pub fn binary() -> PathBuf {
        process::binary("zebrad", "ZEBRAD_BIN")
    }

    /// Launches zebrad and waits until its RPC answers. A fresh regtest is
    /// left at height 1.
    pub async fn launch(config: ZebradConfig) -> Result<Self> {
        let bin = Self::binary();
        let version = version(&bin).await?;
        ZEBRAD_VERSION.get_or_init(|| version.clone());

        let run_dir = tempfile::Builder::new().prefix("zaino-zebrad-").tempdir()?;
        let state_dir = match (&config.network, &config.state_from) {
            (Network::Regtest(_), Some(src)) => {
                let dir = run_dir.path().join("state");
                process::copy_dir(src, &dir).with_context(|| "copy chain cache")?;
                dir
            }
            (Network::Regtest(_), None) => run_dir.path().join("state"),
            (_, Some(dir)) => dir.clone(),
            (_, None) => default_public_state_dir()?,
        };
        let config_path = run_dir.path().join("zebrad.toml");
        std::fs::write(&config_path, render_config(&config, &state_dir)?)?;

        let mut process = Process::spawn(
            "zebrad",
            &bin,
            ["-c", &config_path.display().to_string(), "start"],
            run_dir.path(),
            Stdio::null(),
        )?;
        let rpc_addr = bound_addr(
            process
                .wait_for_line(RPC_OPENED, FATAL, START_TIMEOUT)
                .await?,
        )?;
        let indexer_grpc_addr = bound_addr(
            process
                .wait_for_line(INDEXER_OPENED, FATAL, START_TIMEOUT)
                .await?,
        )?;
        let rpc = JsonRpcClient::new(rpc_addr);

        let probe = match config.network {
            Network::Regtest(_) => "getblocktemplate",
            _ => "getblockchaininfo",
        };
        process::poll(
            &format!("zebrad `{probe}` answering"),
            Duration::from_millis(50),
            START_TIMEOUT,
            || async { rpc.call_value(probe, json!([])).await.ok() },
        )
        .await
        .with_context(|| process.log_tail())?;

        let zebrad = Self {
            process,
            rpc_addr,
            indexer_grpc_addr,
            state_dir,
            rpc,
            network: config.network.clone(),
            coinbase: config.coinbase,
            _run_dir: run_dir,
        };
        if matches!(config.network, Network::Regtest(_)) && config.state_from.is_none() {
            zebrad.generate_blocks(1).await?;
        }
        if let Some(expected) = config.expect_height {
            let rpc = &zebrad.rpc;
            process::poll(
                &format!("restored zebrad reaching height {expected}"),
                Duration::from_millis(100),
                START_TIMEOUT,
                || async {
                    let height = chain_height(rpc).await.ok()?;
                    (height >= expected).then_some(height)
                },
            )
            .await
            .with_context(|| zebrad.process.log_tail())?;
        }
        Ok(zebrad)
    }

    /// Stops this node and launches another on the same ports from the state
    /// at `state_from`, holding `height`: an indexer that held this node's
    /// addresses now talks to a validator on a possibly different chain.
    pub async fn respawn_from(mut self, state_from: &Path, height: u32) -> Result<Self> {
        self.process.stop(STOP_GRACE).await?;
        let config = ZebradConfig {
            network: self.network.clone(),
            coinbase: self.coinbase,
            state_from: Some(state_from.to_path_buf()),
            ports: Some((self.rpc_addr.port(), self.indexer_grpc_addr.port())),
            expect_height: Some(height),
        };
        drop(self);
        Self::launch(config).await
    }

    /// The raw block at `height`, hex, as `submit_block` takes it.
    pub async fn raw_block(&self, height: u32) -> Result<String> {
        let raw = self
            .rpc
            .call_value("getblock", json!([height.to_string(), 0]))
            .await?;
        Ok(raw.as_str().context("getblock 0 returns hex")?.to_string())
    }

    /// Submits a block, which may extend a side chain; the node reorgs to it
    /// once that chain is the longest. Returns the new tip height.
    pub async fn submit_block(&self, hex: &str) -> Result<u32> {
        let reply = self.rpc.call_value("submitblock", json!([hex])).await?;
        if let Some(rejection) = reply.as_str() {
            bail!("submitblock rejected the block: {rejection}");
        }
        self.chain_height().await
    }

    /// The pool coinbase pays into.
    pub fn coinbase_pool(&self) -> Pool {
        self.coinbase
    }

    /// The miner address coinbase pays to.
    pub fn miner_address(&self) -> &'static str {
        self.coinbase.miner_address()
    }

    /// The captured logs.
    pub fn log_tail(&self) -> String {
        self.process.log_tail()
    }

    async fn generate(&self, method: &str, n: u32, params: Value) -> Result<u32> {
        ensure!(
            matches!(self.network, Network::Regtest(_)),
            "blocks can only be generated on regtest"
        );
        let start = self.chain_height().await?;
        let target = start + n;
        self.rpc.call_value(method, params).await?;
        let rpc = &self.rpc;
        process::poll(
            &format!("zebrad reaching height {target}"),
            Duration::from_millis(50),
            Duration::from_secs(30) + Duration::from_secs(u64::from(n)),
            || async {
                let height = chain_height(rpc).await.ok()?;
                (height >= target).then_some(height)
            },
        )
        .await
    }
}

async fn chain_height(rpc: &JsonRpcClient) -> Result<u32> {
    let info = rpc.call_value("getblockchaininfo", json!([])).await?;
    let height = info
        .get("blocks")
        .and_then(Value::as_u64)
        .context("getblockchaininfo.blocks")?;
    Ok(u32::try_from(height)?)
}

impl Validator for Zebrad {
    fn network(&self) -> &Network {
        &self.network
    }

    fn rpc_addr(&self) -> SocketAddr {
        self.rpc_addr
    }

    fn indexer_grpc_addr(&self) -> SocketAddr {
        self.indexer_grpc_addr
    }

    fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn rpc(&self) -> &JsonRpcClient {
        &self.rpc
    }

    async fn chain_height(&self) -> Result<u32> {
        chain_height(&self.rpc).await
    }

    async fn best_block_hash(&self) -> Result<String> {
        let hash = self.rpc.call_value("getbestblockhash", json!([])).await?;
        Ok(hash.as_str().context("getbestblockhash")?.to_string())
    }

    async fn activation_heights(&self) -> Result<ActivationHeights> {
        let info = self.rpc.call_value("getblockchaininfo", json!([])).await?;
        activation_heights_from_info(&info)
    }

    async fn generate_blocks(&self, n: u32) -> Result<u32> {
        self.generate("generate", n, json!([n])).await
    }

    async fn generate_blocks_to(&self, n: u32, address: &str) -> Result<u32> {
        self.generate("generatetoaddress", n, json!([n, address]))
            .await
    }

    async fn snapshot(mut self, dir: &Path) -> Result<()> {
        // zebra writes its non-finalised backup at most every 5 s, so a
        // snapshot right after mining would miss the newest blocks.
        let tip = self.best_block_hash().await?;
        let backup = self
            .state_dir
            .join("non_finalized_state")
            .join("regtest")
            .join(&tip);
        process::poll(
            "zebrad backing up its tip",
            Duration::from_millis(250),
            Duration::from_secs(30),
            || async { backup.exists().then_some(()) },
        )
        .await?;
        self.process.stop(STOP_GRACE).await?;
        process::copy_dir(&self.state_dir, dir)
    }
}

/// Runs `zebrad --version` and checks it against [`ZEBRAD_MIN_VERSION`].
pub async fn version(bin: &Path) -> Result<String> {
    let output = process::run(bin, &["--version".to_string()], None)
        .await
        .with_context(|| {
            format!(
                "zebrad not runnable at {} (set ZEBRAD_BIN or put zebrad on PATH)",
                bin.display()
            )
        })?;
    let version = output.trim().to_string();
    let number = version
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_start_matches('v');
    let core = number.split(['-', '+']).next().unwrap_or(number);
    let mut parts = core.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let parsed = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    ensure!(
        parsed >= ZEBRAD_MIN_VERSION,
        "zebrad {version} is older than the required {}.{}.{}",
        ZEBRAD_MIN_VERSION.0,
        ZEBRAD_MIN_VERSION.1,
        ZEBRAD_MIN_VERSION.2
    );
    Ok(version)
}

fn bound_addr(line: String) -> Result<SocketAddr> {
    line.split_whitespace()
        .last()
        .and_then(|addr| addr.parse().ok())
        .with_context(|| format!("no socket address at the end of {line:?}"))
}

fn default_public_state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ZEBRA_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME unset and ZEBRA_STATE_DIR unset")?;
    Ok(PathBuf::from(home).join(".cache").join("zebra"))
}

/// The `zebrad.toml` for `config`.
pub fn render_config(config: &ZebradConfig, state_dir: &Path) -> Result<String> {
    let (network_name, peers, regtest) = match &config.network {
        Network::Regtest(_) => (
            "Regtest",
            "initial_mainnet_peers = []\ninitial_testnet_peers = []\n",
            true,
        ),
        Network::Testnet => ("Testnet", "", false),
        Network::Mainnet => ("Mainnet", "", false),
    };
    let state_dir = state_dir.display();
    let (rpc_port, indexer_port) = config.ports.unwrap_or((0, 0));
    let mut out = format!(
        "\
[consensus]
checkpoint_sync = true

[mempool]
eviction_memory_time = \"1h\"
tx_cost_limit = 80000000

[network]
cache_dir = false
crawl_new_peer_interval = \"1m 1s\"
{peers}listen_addr = \"127.0.0.1:0\"
max_connections_per_ip = 1
network = \"{network_name}\"
peerset_initial_target_size = 25

[rpc]
debug_force_finished_sync = false
enable_cookie_auth = false
parallel_cpu_threads = 0
listen_addr = \"127.0.0.1:{rpc_port}\"
indexer_listen_addr = \"127.0.0.1:{indexer_port}\"

[state]
cache_dir = \"{state_dir}\"
delete_old_database = true
ephemeral = false

[sync]
checkpoint_verify_concurrency_limit = 1000
download_concurrency_limit = 50
full_verify_concurrency_limit = 20
parallel_cpu_threads = 0

[tracing]
buffer_limit = 128000
force_use_color = false
use_color = false
filter = \"info\"
use_journald = false
"
    );
    if !regtest {
        return Ok(out);
    }
    let Network::Regtest(heights) = &config.network else {
        unreachable!("regtest checked above")
    };
    ensure!(
        heights.canopy == Some(1),
        "zebrad regtest needs Canopy (and everything before it) at height 1"
    );
    out.push_str(&format!(
        "\n[mining]\nminer_address = \"{}\"\n\n[network.testnet_parameters.activation_heights]\n",
        config.coinbase.miner_address()
    ));
    let entries = [
        ("Canopy", heights.canopy),
        ("NU5", heights.nu5),
        ("NU6", heights.nu6),
        ("\"NU6.1\"", heights.nu6_1),
        ("\"NU6.2\"", heights.nu6_2),
        ("\"NU6.3\"", heights.nu6_3),
        ("NU7", heights.nu7),
    ];
    for (key, height) in entries {
        if let Some(height) = height {
            out.push_str(&format!("{key} = {height}\n"));
        }
    }
    // NU6.1's activation block is rejected without a lockbox disbursement and a
    // funding stream that has deposited into the deferred pool.
    out.push_str(&format!(
        "
[[network.testnet_parameters.lockbox_disbursements]]
address = \"{LOCKBOX_ADDRESS}\"
amount = 1

[network.testnet_parameters.post_nu6_funding_streams.height_range]
start = 2
end = 1000000

[[network.testnet_parameters.post_nu6_funding_streams.recipients]]
receiver = \"Deferred\"
numerator = 1
"
    ));
    if heights.nu6.is_none_or(|nu6| nu6 > 2) {
        bail!("the regtest fixture funds the deferred pool from height 2, so NU6 must be at 2");
    }
    Ok(out)
}
