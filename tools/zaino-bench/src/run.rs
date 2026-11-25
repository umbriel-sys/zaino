//! One run: start zainod on the config, watch it until every answer is
//! local, stop it, measure what it used, record it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::IsTerminal as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, Signal, SignalKind};

use crate::metrics;
use crate::record::{Build, Host, Outcome, Record, Results, Sample, Synced, Zainod};
use crate::report;

/// How often `/readyz` is polled.
const POLL: Duration = Duration::from_secs(1);
/// `/metrics` is read once in this many polls.
const POLLS_PER_SAMPLE: u64 = 5;
/// The longest one request to the admin endpoint may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the admin endpoint may go without answering before the run fails.
const ADMIN_SILENCE: Duration = Duration::from_secs(60);
/// How long no index may advance, once the build has started.
const STALL: Duration = Duration::from_secs(600);
/// How long zainod has to exit after SIGINT.
const SHUTDOWN: Duration = Duration::from_secs(120);
/// Lines of zainod's log quoted when it exits early.
const LOG_TAIL_LINES: usize = 15;

/// The step of a run that could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Config,
    Build,
    Host,
    Launch,
    Observe,
    Measure,
    Record,
}

/// The tool could not carry out a step of the run.
#[derive(Debug)]
pub(crate) struct RunError {
    step: Step,
    cause: String,
}

impl RunError {
    fn new(step: Step, cause: impl fmt::Display) -> Self {
        Self {
            step,
            cause: cause.to_string(),
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = match self.step {
            Step::Config => "reading the config",
            Step::Build => "building zainod",
            Step::Host => "describing this machine",
            Step::Launch => "starting zainod",
            Step::Observe => "watching zainod",
            Step::Measure => "measuring the run",
            Step::Record => "recording the run",
        };
        write!(f, "{step}: {}", self.cause)
    }
}

/// What the tool reads from the zainod config. The file itself is passed to
/// zainod unchanged.
#[derive(Debug, PartialEq)]
struct BenchConfig {
    /// The file as it is recorded: verbatim but for the validator's
    /// credentials.
    recorded: String,
    /// `persistence.path`.
    data_dir: PathBuf,
    /// The directories `[indexes.paths]` puts single indexes in.
    index_dirs: Vec<PathBuf>,
    /// `[admin] listen_address`.
    admin: SocketAddr,
    /// The `[indexes]` named, for display.
    indexes: String,
}

impl BenchConfig {
    fn parse(text: &str) -> Result<Self, String> {
        let table: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
        let data_dir = table
            .get("persistence")
            .and_then(|persistence| persistence.get("path"))
            .and_then(toml::Value::as_str)
            .map(PathBuf::from)
            .ok_or(
                "[persistence] path is not set: name the (empty) directory this run syncs into",
            )?;
        let admin = table
            .get("admin")
            .and_then(|admin| admin.get("listen_address"))
            .and_then(toml::Value::as_str)
            .ok_or(
                "[admin] listen_address is not set: the run is timed and analysed through \
                 zainod's admin endpoint",
            )?;
        let admin: SocketAddr = admin
            .parse()
            .map_err(|error| format!("[admin] listen_address `{admin}`: {error}"))?;
        let indexes = table.get("indexes");
        let index_dirs = indexes
            .and_then(|indexes| indexes.get("paths"))
            .and_then(toml::Value::as_table)
            .map(|paths| {
                paths
                    .values()
                    .filter_map(toml::Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            recorded: redact_credentials(text)?,
            data_dir,
            index_dirs,
            admin,
            indexes: indexes_label(indexes),
        })
    }

    /// Every directory the run writes indexes to.
    fn store_dirs(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.data_dir.as_path()).chain(self.index_dirs.iter().map(PathBuf::as_path))
    }

    /// Where the admin endpoint is reached: loopback when it listens on every
    /// address.
    fn admin_url(&self, path: &str) -> String {
        let mut address = self.admin;
        if address.ip().is_unspecified() {
            address.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        format!("http://{address}{path}")
    }
}

/// What a recorded config shows in place of a credential.
const REDACTED: &str = "<redacted>";
/// The validator credentials a config may hold.
const CREDENTIALS: [&str; 2] = ["user", "password"];

/// The validator credentials `config` holds that are not redacted.
fn exposed_credentials(config: &toml::Table) -> Vec<&'static str> {
    CREDENTIALS
        .into_iter()
        .filter(|key| {
            config
                .get("validator")
                .and_then(|validator| validator.get(key))
                .is_some_and(|value| value.as_str() != Some(REDACTED))
        })
        .collect()
}

/// `text` with the values of `[validator]`'s `user` and `password` replaced:
/// the copy that is recorded (zainod reads the file itself). Refused when a
/// credential is written some way this cannot find, so none is ever recorded.
fn redact_credentials(text: &str) -> Result<String, String> {
    let mut table = String::new();
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let content = line.trim_start();
        if let Some(header) = content.strip_prefix('[') {
            table = header
                .split(']')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
        }
        let key = content.split_once('=').map(|(key, _)| key.trim());
        let credential = key.filter(|key| {
            let path = match table.is_empty() {
                true => (*key).to_owned(),
                false => format!("{table}.{key}"),
            };
            CREDENTIALS
                .iter()
                .any(|credential| path == format!("validator.{credential}"))
        });
        match credential {
            Some(key) => {
                let indent = line.len().saturating_sub(content.len());
                let ending = if line.ends_with('\n') { "\n" } else { "" };
                out.push_str(line.get(..indent).unwrap_or_default());
                out.push_str(&format!("{key} = \"{REDACTED}\"{ending}"));
            }
            None => out.push_str(line),
        }
    }
    let redacted: toml::Table = toml::from_str(&out).map_err(|error| {
        format!("the config does not parse with its credentials redacted: {error}")
    })?;
    let exposed = exposed_credentials(&redacted);
    if !exposed.is_empty() {
        return Err(format!(
            "[validator] {} would be recorded in the clear: write it on a line of its own \
             under [validator], as `{} = \"...\"`",
            exposed.join(" and "),
            exposed.first().copied().unwrap_or("password"),
        ));
    }
    Ok(out)
}

/// The `[indexes]` table as a short label: `full`, `lightclient +spent`.
fn indexes_label(indexes: Option<&toml::Value>) -> String {
    let names = |key: &str| -> Vec<String> {
        indexes
            .and_then(|indexes| indexes.get(key))
            .and_then(toml::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut label = indexes
        .and_then(|indexes| indexes.get("set"))
        .and_then(toml::Value::as_str)
        .unwrap_or("default")
        .to_owned();
    for (sign, key) in [('+', "enable"), ('-', "disable")] {
        let names = names(key);
        if !names.is_empty() {
            label.push_str(&format!(" {sign}{}", names.join(",")));
        }
    }
    label
}

/// Whether `dir` holds nothing: absent, or an empty directory.
fn is_empty(dir: &Path) -> Result<bool, String> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("{}: {error}", dir.display())),
    }
}

/// What `/readyz` answers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Readyz {
    ready: bool,
    local: bool,
    components: Vec<Component>,
    indexes: Vec<Index>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Component {
    name: String,
    health: String,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Index {
    name: String,
    state: String,
    height: Option<u32>,
}

impl Readyz {
    fn parse(body: &str) -> Result<Self, String> {
        serde_json::from_str(body).map_err(|error| format!("/readyz: {error}"))
    }

    /// The component that has failed for good, with its reason.
    fn critical(&self) -> Option<String> {
        self.components
            .iter()
            .find(|component| component.health == "Critical")
            .map(|component| {
                format!(
                    "{} is critical: {}",
                    component.name,
                    component.reason.as_deref().unwrap_or("no reason given")
                )
            })
    }

    /// The indexes the config builds.
    fn built_indexes(&self) -> impl Iterator<Item = &Index> {
        self.indexes.iter().filter(|index| index.state != "absent")
    }

    /// The lowest index and its height; `None` when no index is built.
    fn slowest(&self) -> Option<(&str, Option<u32>)> {
        self.built_indexes()
            .min_by_key(|index| index.height)
            .map(|index| (index.name.as_str(), index.height))
    }

    /// The height every built index holds; `None` when none is built or one
    /// holds no block yet.
    fn watermark(&self) -> Option<u32> {
        self.slowest().and_then(|(_, height)| height)
    }

    fn heights(&self) -> Vec<Option<u32>> {
        self.built_indexes().map(|index| index.height).collect()
    }
}

/// Runs `program` to its end and returns its standard output.
async fn output_of(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .await
        .map_err(|error| format!("`{program}` did not run: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn sha256(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or("the path is not UTF-8")?;
    let output = output_of("sha256sum", &[path], None).await?;
    output
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum printed nothing".to_owned())
}

/// The zainod to run: `prebuilt`, or this checkout's, built now.
async fn zainod(prebuilt: Option<PathBuf>) -> Result<Zainod, RunError> {
    let failed = |cause: String| RunError::new(Step::Build, cause);
    let (binary, build) = match prebuilt {
        Some(binary) => (binary, None),
        None => {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "-p",
                    "zainod",
                    "--features",
                    "no_tls_with_prometheus",
                ])
                .current_dir(&root)
                .stdin(Stdio::null())
                .status()
                .await
                .map_err(|error| failed(format!("cargo did not run: {error}")))?;
            if !status.success() {
                return Err(failed(format!("cargo build failed ({status})")));
            }
            let metadata = output_of(
                "cargo",
                &["metadata", "--format-version", "1", "--no-deps"],
                Some(&root),
            )
            .await
            .map_err(failed)?;
            let metadata: serde_json::Value = serde_json::from_str(&metadata)
                .map_err(|error| failed(format!("cargo metadata: {error}")))?;
            let target = metadata
                .get("target_directory")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| failed("cargo metadata names no target directory".to_owned()))?;
            let commit = output_of("git", &["rev-parse", "HEAD"], Some(&root))
                .await
                .map_err(failed)?;
            let changes = output_of("git", &["status", "--porcelain"], Some(&root))
                .await
                .map_err(failed)?;
            (
                Path::new(target).join("release").join("zainod"),
                Some(Build {
                    commit: commit.trim().to_owned(),
                    dirty: !changes.trim().is_empty(),
                }),
            )
        }
    };
    let binary = fs::canonicalize(&binary)
        .map_err(|error| failed(format!("{}: {error}", binary.display())))?;
    let sha256 = sha256(&binary)
        .await
        .map_err(|cause| failed(format!("{}: {cause}", binary.display())))?;
    Ok(Zainod {
        binary,
        sha256,
        build,
    })
}

/// The value of `key:` in a `/proc` file of `key: value` lines.
fn proc_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then_some(value.trim())
    })
}

/// A `/proc` size field (`1234 kB`) in bytes.
fn proc_kib(text: &str, key: &str) -> Option<u64> {
    proc_field(text, key)?
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

/// The type of the filesystem holding `dir`, from the mount table.
fn filesystem_of(mounts: &str, dir: &Path) -> Option<String> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (_device, mount, kind) = (fields.next()?, fields.next()?, fields.next()?);
            dir.starts_with(mount).then_some((mount.len(), kind))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, kind)| kind.to_owned())
}

/// `dir`, or its nearest ancestor that exists, resolved.
fn nearest_existing(dir: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(dir).map_err(|error| error.to_string())?;
    absolute
        .ancestors()
        .find_map(|ancestor| fs::canonicalize(ancestor).ok())
        .ok_or_else(|| format!("no ancestor of {} exists", dir.display()))
}

fn host(data_dir: &Path) -> Result<Host, RunError> {
    let failed = |cause: String| RunError::new(Step::Host, cause);
    let read =
        |path: &str| fs::read_to_string(path).map_err(|error| failed(format!("{path}: {error}")));
    let cpuinfo = read("/proc/cpuinfo")?;
    let meminfo = read("/proc/meminfo")?;
    let mounts = read("/proc/mounts")?;
    let kernel = read("/proc/sys/kernel/osrelease")?;
    let data_dir = nearest_existing(data_dir).map_err(failed)?;
    Ok(Host {
        cpu: proc_field(&cpuinfo, "model name")
            .ok_or_else(|| failed("/proc/cpuinfo names no CPU model".to_owned()))?
            .to_owned(),
        cores: std::thread::available_parallelism()
            .map_err(|error| failed(format!("core count: {error}")))?
            .get(),
        ram_bytes: proc_kib(&meminfo, "MemTotal")
            .ok_or_else(|| failed("/proc/meminfo has no MemTotal".to_owned()))?,
        kernel: kernel.trim().to_owned(),
        filesystem: filesystem_of(&mounts, &data_dir)
            .ok_or_else(|| failed(format!("no mount holds {}", data_dir.display())))?,
    })
}

/// The bytes of every file under `dir`; 0 when it does not exist.
fn disk_bytes(dir: &Path) -> Result<u64, String> {
    let at = |error: std::io::Error| format!("{}: {error}", dir.display());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(at(error)),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry.map_err(at)?;
        let kind = entry.file_type().map_err(at)?;
        let bytes = if kind.is_dir() {
            disk_bytes(&entry.path())?
        } else if kind.is_file() {
            entry.metadata().map_err(at)?.len()
        } else {
            0
        };
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// The directories directly under `dir`.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let at = |error: std::io::Error| format!("{}: {error}", dir.display());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(at(error)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(at)?;
        if entry.file_type().map_err(at)?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// Disk used: in total, and by each index directory. Indexes are at
/// `<data dir>/<network>/indexes/<index>`, and directly under each
/// `[indexes.paths]` directory.
fn disk(config: &BenchConfig) -> Result<(u64, BTreeMap<String, u64>), String> {
    let mut total = 0u64;
    for dir in config.store_dirs() {
        total = total.saturating_add(disk_bytes(dir)?);
    }
    let mut index_dirs = Vec::new();
    for network in subdirs(&config.data_dir)? {
        index_dirs.extend(subdirs(&network.join("indexes"))?);
    }
    for dir in &config.index_dirs {
        index_dirs.extend(subdirs(dir)?);
    }
    let mut by_index = BTreeMap::new();
    for dir in index_dirs {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no name", dir.display()))?;
        let bytes: &mut u64 = by_index.entry(name).or_default();
        *bytes = bytes.saturating_add(disk_bytes(&dir)?);
    }
    Ok((total, by_index))
}

/// The last lines of zainod's log.
fn log_tail(log: &Path) -> String {
    match fs::read_to_string(log) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let skip = lines.len().saturating_sub(LOG_TAIL_LINES);
            lines.into_iter().skip(skip).collect::<Vec<_>>().join("\n")
        }
        Err(error) => format!("({} could not be read: {error})", log.display()),
    }
}

/// What a sync that reached `local` measured while zainod was running.
struct Local {
    sync_secs: f64,
    height: Option<u32>,
    last: Sample,
    peak_memory_bytes: u64,
}

/// What watching zainod came to.
struct Observed {
    ready_secs: Option<f64>,
    /// The sync's measurements, or why the run failed.
    result: Result<Local, String>,
}

/// One running zainod and where its run is recorded.
struct Running<'a> {
    child: Child,
    started: Instant,
    config: &'a BenchConfig,
    results: &'a Results,
    run_id: &'a str,
    log: PathBuf,
    http: reqwest::Client,
    /// SIGINT and SIGTERM sent to this tool: zainod is then stopped and the
    /// run recorded as failed, never left running.
    interrupt: Signal,
    terminate: Signal,
}

impl Running<'_> {
    async fn get(&self, path: &str) -> Result<String, String> {
        let response = self
            .http
            .get(self.config.admin_url(path))
            .send()
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        // `/readyz` answers 503 with the same body until zainod is serving
        if status.is_success() || status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            Ok(body)
        } else {
            Err(format!("{path} answered {status}"))
        }
    }

    /// Reads `/metrics` and appends the sample to the run's series.
    async fn sample(&self) -> Result<Sample, String> {
        let body = self.get("/metrics").await?;
        let sample = metrics::parse(&body, self.started.elapsed().as_secs_f64())
            .map_err(|error| error.to_string())?;
        self.results
            .append_sample(self.run_id, &sample)
            .map_err(|error| error.to_string())?;
        Ok(sample)
    }

    /// zainod's peak resident memory so far.
    fn peak_memory(&self) -> Result<u64, String> {
        let pid = self.child.id().ok_or("zainod has exited")?;
        let path = format!("/proc/{pid}/status");
        let status = fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
        proc_kib(&status, "VmHWM").ok_or_else(|| format!("{path} has no VmHWM"))
    }

    /// Watches zainod until every answer is local, or the run fails.
    async fn observe(&mut self, timeout: Duration) -> Result<Observed, RunError> {
        let mut ticker = tokio::time::interval(POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ready_secs = None;
        let mut last_answer = Instant::now();
        // the indexes' heights and when one last moved, once the build has started
        let mut progress: Option<(Vec<Option<u32>>, Instant)> = None;
        let mut samples: (Option<Sample>, Option<Sample>) = (None, None);
        let mut polls = 0u64;
        let failed = |ready_secs, cause: String| {
            Ok(Observed {
                ready_secs,
                result: Err(cause),
            })
        };
        loop {
            let signal = tokio::select! {
                _ = ticker.tick() => None,
                _ = self.interrupt.recv() => Some("SIGINT"),
                _ = self.terminate.recv() => Some("SIGTERM"),
            };
            if let Some(signal) = signal {
                return failed(
                    ready_secs,
                    format!("zaino-bench was interrupted ({signal}) before the sync finished"),
                );
            }
            polls = polls.saturating_add(1);
            let exited = self
                .child
                .try_wait()
                .map_err(|error| RunError::new(Step::Observe, error))?;
            if let Some(status) = exited {
                return failed(
                    ready_secs,
                    format!(
                        "zainod exited ({status}) before the sync finished; its log ends:\n{}",
                        log_tail(&self.log)
                    ),
                );
            }
            if self.started.elapsed() >= timeout {
                return failed(
                    ready_secs,
                    format!("the sync did not finish within --timeout ({timeout:?})"),
                );
            }
            let readyz = match self
                .get("/readyz")
                .await
                .and_then(|body| Readyz::parse(&body))
            {
                Ok(readyz) => {
                    last_answer = Instant::now();
                    readyz
                }
                Err(error) if last_answer.elapsed() >= ADMIN_SILENCE => {
                    return failed(
                        ready_secs,
                        format!(
                            "zainod's admin endpoint has not answered for {}s: {error}",
                            ADMIN_SILENCE.as_secs()
                        ),
                    );
                }
                Err(_) => {
                    status_line(&format!(
                        "waiting for zainod's admin endpoint  {}",
                        report::duration(self.started.elapsed().as_secs_f64())
                    ));
                    continue;
                }
            };
            if let Some(critical) = readyz.critical() {
                return failed(ready_secs, critical);
            }
            let elapsed = self.started.elapsed().as_secs_f64();
            if readyz.ready && ready_secs.is_none() {
                ready_secs = Some(elapsed);
            }
            if readyz.local {
                let last = match self.sample().await {
                    Ok(sample) => sample,
                    Err(cause) => return failed(ready_secs, cause),
                };
                let peak_memory_bytes = match self.peak_memory() {
                    Ok(bytes) => bytes,
                    Err(cause) => return failed(ready_secs, cause),
                };
                return Ok(Observed {
                    ready_secs,
                    result: Ok(Local {
                        sync_secs: elapsed,
                        height: readyz.watermark(),
                        last,
                        peak_memory_bytes,
                    }),
                });
            }
            // `/readyz` lists the indexes only once zainod serves; until then
            // the last `/metrics` sample has their heights
            let mut heights = readyz.heights();
            if heights.is_empty() {
                if let Some(sample) = &samples.1 {
                    heights = sample.heights.values().copied().map(Some).collect();
                }
            }
            if !heights.is_empty() {
                match &mut progress {
                    Some((seen, moved)) if *seen == heights => {
                        if moved.elapsed() >= STALL {
                            return failed(
                                ready_secs,
                                format!(
                                    "no index has advanced for {} minutes",
                                    STALL.as_secs() / 60
                                ),
                            );
                        }
                    }
                    _ => progress = Some((heights, Instant::now())),
                }
            }
            if polls.is_multiple_of(POLLS_PER_SAMPLE) {
                match self.sample().await {
                    Ok(sample) => samples = (samples.1.take(), Some(sample)),
                    Err(cause) => return failed(ready_secs, cause),
                }
            }
            status_line(&progress_line(&readyz, &samples, elapsed));
        }
    }

    /// Stops zainod with SIGINT: it must exit 0 within [`SHUTDOWN`]; one that
    /// does not is killed.
    async fn stop(&mut self) -> Result<(), String> {
        let Some(pid) = self.child.id() else {
            return match self.child.wait().await {
                Ok(status) => Err(format!("zainod had already exited ({status})")),
                Err(error) => Err(format!("zainod had already exited: {error}")),
            };
        };
        output_of("kill", &["-INT", &pid.to_string()], None).await?;
        match tokio::time::timeout(SHUTDOWN, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(format!(
                "zainod did not shut down cleanly ({status}); its log ends:\n{}",
                log_tail(&self.log)
            )),
            Ok(Err(error)) => Err(format!("waiting for zainod to exit: {error}")),
            Err(_) => {
                let killed = match self.child.kill().await {
                    Ok(()) => "killed".to_owned(),
                    Err(error) => format!("and could not be killed: {error}"),
                };
                Err(format!(
                    "zainod did not exit within {}s of SIGINT; {killed}",
                    SHUTDOWN.as_secs()
                ))
            }
        }
    }
}

/// Rewrites the terminal's status line; nothing when not on a terminal.
fn status_line(line: &str) {
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K{line}");
    }
}

/// The status line of a sync in progress.
fn progress_line(
    readyz: &Readyz,
    samples: &(Option<Sample>, Option<Sample>),
    elapsed_secs: f64,
) -> String {
    let elapsed = report::duration(elapsed_secs);
    // `/readyz` lists the indexes only once zainod serves; the store builds
    // before that too, and `/metrics` shows it
    let from_readyz = readyz
        .slowest()
        .map(|(name, height)| (name.to_owned(), height));
    let from_metrics = || {
        let sample = samples.1.as_ref()?;
        let (name, _) = sample.heights.iter().min_by_key(|(_, height)| **height)?;
        Some((name.clone(), sample.watermark))
    };
    let Some((slowest, height)) = from_readyz.or_else(from_metrics) else {
        let waiting = match readyz.ready {
            true => "serving; no index is built",
            false => "starting: connecting to the validator and building the chain head",
        };
        return format!("{waiting}  {elapsed}");
    };
    let target = samples.1.as_ref().and_then(|sample| sample.target);
    let done = match (height, target) {
        (Some(height), Some(target)) => format!(
            "{:.1}%  height {} / {}",
            100.0 * (f64::from(height) + 1.0) / (f64::from(target) + 1.0),
            report::count(u64::from(height)),
            report::count(u64::from(target)),
        ),
        (Some(height), None) => format!("height {}", report::count(u64::from(height))),
        (None, _) => "no block built yet".to_owned(),
    };
    let rate = match samples {
        (Some(earlier), Some(later)) => {
            let secs = later.elapsed_secs - earlier.elapsed_secs;
            let blocks = later
                .watermark
                .unwrap_or(0)
                .saturating_sub(earlier.watermark.unwrap_or(0));
            match secs > 0.0 {
                true => format!(
                    "  {} blk/s",
                    report::count((f64::from(blocks) / secs) as u64)
                ),
                false => String::new(),
            }
        }
        _ => String::new(),
    };
    format!("{done}{rate}  slowest: {slowest}  {elapsed}")
}

/// Runs zainod on the config at `config_path` and records the run. `Ok(true)`
/// when the sync completed, `Ok(false)` when the run failed and was recorded
/// as failed.
pub(crate) async fn run(
    config_path: &Path,
    prebuilt: Option<PathBuf>,
    timeout: Duration,
    results: &Results,
) -> Result<bool, RunError> {
    let config_error = |cause: String| RunError::new(Step::Config, cause);
    let text = fs::read_to_string(config_path)
        .map_err(|error| config_error(format!("{}: {error}", config_path.display())))?;
    let config = BenchConfig::parse(&text)
        .map_err(|cause| config_error(format!("{}: {cause}", config_path.display())))?;
    for dir in config.store_dirs() {
        if !is_empty(dir).map_err(config_error)? {
            return Err(config_error(format!(
                "{} is not empty: a run syncs from nothing, so point the config at an empty \
                 directory",
                dir.display()
            )));
        }
    }
    let zainod = zainod(prebuilt).await?;
    let host = host(&config.data_dir)?;

    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RunError::new(Step::Launch, format!("the clock: {error}")))?
        .as_secs();
    let run_id = format!("run-{started_unix}");
    let run_dir = results
        .create_run_dir(&run_id)
        .map_err(|error| RunError::new(Step::Record, error))?;
    let log = run_dir.join("zainod.log");
    let launch_error = |cause: String| RunError::new(Step::Launch, cause);
    let log_file = fs::File::create(&log)
        .map_err(|error| launch_error(format!("{}: {error}", log.display())))?;
    let log_errors = log_file
        .try_clone()
        .map_err(|error| launch_error(format!("{}: {error}", log.display())))?;
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| launch_error(format!("the HTTP client: {error}")))?;

    // A cleared environment: `ZAINO_*` variables override the config file.
    // `HOME` and `XDG_CACHE_HOME` stay, as zainod's default paths come from them;
    // the log goes to a file, so without colour codes.
    let mut command = Command::new(&zainod.binary);
    command
        .arg("start")
        .arg("--config")
        .arg(config_path)
        .env_clear()
        .env("ZAINOLOG_COLOR", "0")
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_errors)
        .kill_on_drop(true);
    for kept in ["HOME", "XDG_CACHE_HOME", "PATH"] {
        if let Some(value) = std::env::var_os(kept) {
            command.env(kept, value);
        }
    }
    eprintln!(
        "zaino-bench {run_id}: {} on indexes `{}`; log {}",
        zainod.binary.display(),
        config.indexes,
        log.display()
    );
    // from here a signal to this tool stops zainod and records the run
    let on = |kind: SignalKind| {
        signal(kind).map_err(|error| launch_error(format!("listening for signals: {error}")))
    };
    let (interrupt, terminate) = (on(SignalKind::interrupt())?, on(SignalKind::terminate())?);
    let child = command
        .spawn()
        .map_err(|error| launch_error(format!("{}: {error}", zainod.binary.display())))?;
    let mut running = Running {
        interrupt,
        terminate,
        child,
        started: Instant::now(),
        config: &config,
        results,
        run_id: &run_id,
        log,
        http,
    };

    let observed = running.observe(timeout).await?;
    if std::io::stderr().is_terminal() {
        eprintln!();
    }
    let had_exited = running.child.id().is_none();
    let stopped = running.stop().await;
    let outcome = match (observed.result, stopped) {
        (Ok(local), Ok(())) => {
            let (disk_total_bytes, disk_bytes) =
                disk(&config).map_err(|cause| RunError::new(Step::Measure, cause))?;
            match local.last.cpu_secs {
                Some(cpu_secs) => Outcome::Synced(Synced {
                    sync_secs: local.sync_secs,
                    height: local.height,
                    peak_memory_bytes: local.peak_memory_bytes,
                    cpu_secs,
                    disk_total_bytes,
                    disk_bytes,
                    stages: local.last.stages,
                }),
                None => Outcome::Failed {
                    cause: "zainod's /metrics reports no CPU time".to_owned(),
                },
            }
        }
        (Ok(_), Err(cause)) => Outcome::Failed { cause },
        (Err(cause), Ok(())) => Outcome::Failed { cause },
        // the exit is the failure already reported
        (Err(cause), Err(_)) if had_exited => Outcome::Failed { cause },
        (Err(cause), Err(stop)) => Outcome::Failed {
            cause: format!("{cause}\n{stop}"),
        },
    };
    let record = Record {
        run_id,
        started_unix,
        indexes: config.indexes.clone(),
        config: config.recorded.clone(),
        zainod,
        host,
        ready_secs: observed.ready_secs,
        outcome,
    };
    results
        .append_record(&record)
        .map_err(|error| RunError::new(Step::Record, error))?;
    let series = results
        .series(&record.run_id)
        .map_err(|error| RunError::new(Step::Record, error))?;
    print!("{}", report::run_summary(&record, &series));
    Ok(matches!(record.outcome, Outcome::Synced(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTING: &str = include_str!("../fixtures/readyz-starting.json");
    const LOCAL: &str = include_str!("../fixtures/readyz-local.json");
    const LOCAL_LIGHTCLIENT: &str = include_str!("../fixtures/readyz-local-lightclient.json");

    const CONFIG: &str = r#"
network = "Regtest"

[indexes]
set = "lightclient"
enable = ["spent", "history_tree"]

[indexes.paths]
compact_block = "/mnt/fast/zaino"

[persistence]
path = "/data/zaino"

[admin]
listen_address = "0.0.0.0:9998"
"#;

    #[test]
    fn the_config_names_the_store_the_admin_endpoint_and_the_indexes() {
        let config = BenchConfig::parse(CONFIG).expect("parses");
        assert_eq!(config.recorded, CONFIG, "no credentials: verbatim");
        assert_eq!(config.data_dir, Path::new("/data/zaino"));
        assert_eq!(config.index_dirs, [PathBuf::from("/mnt/fast/zaino")]);
        assert_eq!(config.indexes, "lightclient +spent,history_tree");
        assert_eq!(
            config.admin_url("/readyz"),
            "http://127.0.0.1:9998/readyz",
            "an endpoint on every address is reached on loopback"
        );
        assert_eq!(
            config.store_dirs().collect::<Vec<_>>(),
            [Path::new("/data/zaino"), Path::new("/mnt/fast/zaino")]
        );
    }

    #[test]
    fn the_validators_credentials_are_redacted_in_the_recorded_config() {
        let config = "\
network = \"Regtest\"
[validator]
rpc_address = \"127.0.0.1:8232\"
  user = \"alice\"   # the rpc user
password=\"hunter2\"
cookie_path = \"/run/zebra/.cookie\"
[validator.read_state]
state_path = \"/password\"
[serve.jsonrpc]
user = \"kept\"";
        let recorded = redact_credentials(config).expect("redacts");
        assert_eq!(
            recorded,
            "\
network = \"Regtest\"
[validator]
rpc_address = \"127.0.0.1:8232\"
  user = \"<redacted>\"
password = \"<redacted>\"
cookie_path = \"/run/zebra/.cookie\"
[validator.read_state]
state_path = \"/password\"
[serve.jsonrpc]
user = \"kept\""
        );
        assert!(!recorded.contains("alice") && !recorded.contains("hunter2"));
        // redacting twice changes nothing, and dotted keys are found too
        assert_eq!(redact_credentials(&recorded).expect("redacts"), recorded);
        assert_eq!(
            redact_credentials("validator.password = \"hunter2\"\n").expect("redacts"),
            "validator.password = \"<redacted>\"\n"
        );
    }

    #[test]
    fn a_credential_that_cannot_be_redacted_refuses_the_config() {
        for config in [
            "validator = { password = \"hunter2\" }\n",
            "[validator]\n\"password\" = \"hunter2\"\n",
        ] {
            let refused = redact_credentials(config).expect_err("in the clear");
            assert!(refused.contains("password"), "{refused}");
            assert!(!refused.contains("hunter2"), "{refused}");
        }
    }

    #[test]
    fn a_config_without_a_store_path_or_an_admin_endpoint_is_refused() {
        let no_admin = "[persistence]\npath = \"/data\"\n";
        let no_path = "[admin]\nlisten_address = \"127.0.0.1:9998\"\n";
        let bad_admin = "[persistence]\npath = \"/data\"\n[admin]\nlisten_address = \"nowhere\"\n";
        assert!(BenchConfig::parse(no_admin)
            .expect_err("no admin")
            .contains("[admin]"));
        assert!(BenchConfig::parse(no_path)
            .expect_err("no path")
            .contains("[persistence]"));
        assert!(BenchConfig::parse(bad_admin).is_err());
        assert!(BenchConfig::parse("not toml =").is_err());
        let plain = format!("{no_path}[persistence]\npath = \"/data\"\n");
        assert_eq!(
            BenchConfig::parse(&plain).expect("parses").indexes,
            "default"
        );
    }

    #[test]
    fn readyz_before_zainod_serves_is_not_ready_and_lists_nothing() {
        let readyz = Readyz::parse(STARTING).expect("parses");
        assert!(!readyz.ready && !readyz.local);
        assert_eq!(readyz.slowest(), None);
        assert_eq!(readyz.critical(), None);
        assert!(readyz.heights().is_empty());
    }

    /// Not a capture: the regtest fixture's store is built before zainod
    /// serves, so `/readyz` never shows it mid-build.
    #[test]
    fn readyz_of_a_building_store_names_the_lowest_index() {
        let readyz = Readyz::parse(
            r#"{"ready":true,"local":false,"components":[],"indexes":[
                {"name":"headers","state":"building","height":900},
                {"name":"txids","state":"building","height":640},
                {"name":"spent","state":"absent","height":null},
                {"name":"compact_block","state":"building","height":700}]}"#,
        )
        .expect("parses");
        assert_eq!(readyz.slowest(), Some(("txids", Some(640))));
        assert_eq!(readyz.watermark(), Some(640));
        assert_eq!(readyz.heights(), [Some(900), Some(640), Some(700)]);

        let empty = Readyz::parse(
            r#"{"ready":true,"local":false,"components":[],"indexes":[
                {"name":"headers","state":"building","height":900},
                {"name":"txids","state":"building","height":null}]}"#,
        )
        .expect("parses");
        assert_eq!(empty.slowest(), Some(("txids", None)));
        assert_eq!(empty.watermark(), None, "an index holds no block yet");
    }

    #[test]
    fn readyz_of_a_built_store_is_local_at_one_height() {
        for (capture, absent) in [(LOCAL, 0), (LOCAL_LIGHTCLIENT, 4)] {
            let readyz = Readyz::parse(capture).expect("parses");
            assert!(readyz.ready && readyz.local);
            assert_eq!(readyz.critical(), None);
            let height = readyz.watermark().expect("built");
            assert!(readyz
                .built_indexes()
                .all(|index| index.state == "built" && index.height == Some(height)));
            let built = readyz.built_indexes().count();
            assert_eq!(readyz.indexes.len() - built, absent);
        }
    }

    #[test]
    fn a_critical_component_is_named_with_its_reason() {
        let readyz = Readyz::parse(
            r#"{"ready":false,"local":false,"indexes":[],"components":[
                {"name":"mempool","lifecycle":"Ready","health":"Healthy","reason":null},
                {"name":"chain store","lifecycle":"Offline","health":"Critical","reason":"disk full"}]}"#,
        )
        .expect("parses");
        assert_eq!(
            readyz.critical().as_deref(),
            Some("chain store is critical: disk full")
        );
        assert!(Readyz::parse("{}").is_err());
    }

    #[test]
    fn proc_files_are_read() {
        let status = "Name:\tzainod\nVmHWM:\t  204800 kB\nThreads:\t9\n";
        assert_eq!(proc_kib(status, "VmHWM"), Some(204_800 * 1024));
        assert_eq!(proc_kib(status, "VmRSS"), None);
        assert_eq!(proc_field(status, "Name"), Some("zainod"));
        let mounts =
            "/dev/a / ext4 rw 0 0\n/dev/b /mnt/fast btrfs rw 0 0\nproc /proc proc rw 0 0\n";
        let filesystem = |dir: &str| filesystem_of(mounts, Path::new(dir));
        assert_eq!(filesystem("/mnt/fast/zaino").as_deref(), Some("btrfs"));
        assert_eq!(filesystem("/mnt/faster").as_deref(), Some("ext4"));
        assert_eq!(filesystem("/home/x").as_deref(), Some("ext4"));
    }

    #[test]
    fn disk_is_measured_in_total_and_per_index() {
        let root =
            std::env::temp_dir().join(format!("zaino-bench-test-disk-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("the old scratch directory is removed");
        }
        let indexes = root.join("store/regtest/indexes");
        let moved = root.join("fast");
        for (dir, file, bytes) in [
            (indexes.join("headers_2"), "a", 10),
            (indexes.join("headers_2/tables"), "b", 5),
            (indexes.join("txids_1"), "a", 7),
            (moved.join("compact_block_1"), "a", 100),
            (root.join("store/regtest"), "PRIMARY", 3),
        ] {
            fs::create_dir_all(&dir).expect("created");
            fs::write(dir.join(file), vec![0u8; bytes]).expect("written");
        }
        let config = BenchConfig {
            recorded: String::new(),
            data_dir: root.join("store"),
            index_dirs: vec![moved, root.join("never-created")],
            admin: SocketAddr::from(([127, 0, 0, 1], 1)),
            indexes: String::new(),
        };
        assert!(!is_empty(&config.data_dir).expect("read"));
        assert!(is_empty(&root.join("never-created")).expect("absent is empty"));
        let (total, by_index) = disk(&config).expect("measured");
        assert_eq!(total, 125);
        assert_eq!(
            by_index,
            BTreeMap::from([
                ("compact_block_1".to_owned(), 100),
                ("headers_2".to_owned(), 15),
                ("txids_1".to_owned(), 7),
            ])
        );
        fs::remove_dir_all(&root).expect("the scratch directory is removed");
    }
}
