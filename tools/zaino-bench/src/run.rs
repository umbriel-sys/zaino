//! One run: start zainod on the config, watch it until every answer is
//! local, stop it, measure what it used, record it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::IsTerminal as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, Signal, SignalKind};

use crate::metrics;
use crate::record::{
    Build, Disk, DiskUse, Host, Outcome, Record, Results, Sample, Shutdown, Synced, Zainod,
};
use crate::report;

/// How often `/readyz` is polled.
const POLL: Duration = Duration::from_secs(1);
/// Once the build has started, `/metrics` is read once in this many polls
/// (before that, at every poll, to find the build's start to the second).
const POLLS_PER_SAMPLE: u64 = 5;
/// The longest one request to the admin endpoint may take. Not a bound on
/// the run: a request that fails is asked again at the next poll.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `/readyz`, or `/metrics`, may go unread before the run fails.
const ADMIN_SILENCE: Duration = Duration::from_secs(300);
/// How long zainod has to build its first block.
const FIRST_BLOCK: Duration = Duration::from_secs(1_800);
/// How long no index may advance, once the build has started.
const STALL: Duration = Duration::from_secs(600);
/// How long zainod has to exit after SIGINT.
const SHUTDOWN: Duration = Duration::from_secs(300);
/// How long building zainod may take.
const BUILD: Duration = Duration::from_secs(7_200);
/// How long a helper program (`git`, `sha256sum`, `kill`, `cargo metadata`)
/// may take.
const HELPER: Duration = Duration::from_secs(300);
/// How many times a run id already taken is waited out, a second each.
const RUN_ID_ATTEMPTS: u32 = 5;
/// Lines of zainod's log quoted when it exits early.
const LOG_TAIL_LINES: usize = 15;
/// The util-linux program zainod is started through, for its `--pdeathsig`.
const PARENT_DEATH_LAUNCHER: &str = "setpriv";
/// The environment zainod keeps: what its default paths come from, and
/// `PATH`. Everything else is cleared, so no `ZAINO_*` variable overrides
/// the config.
const KEPT_ENVIRONMENT: [&str; 4] = ["HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "PATH"];

/// The step of a run that could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Config,
    Build,
    Host,
    Launch,
    Record,
    Summary,
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
            Step::Record => "recording the run",
            Step::Summary => "printing the run's summary (the run is recorded)",
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
        let table: toml::Table = toml::from_str(text).map_err(|error| toml_fault(text, &error))?;
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

/// A TOML fault as its line and message, without the excerpt of the file the
/// parser prints with it: the excerpt can be a line holding a credential.
fn toml_fault(text: &str, error: &toml::de::Error) -> String {
    let line = error
        .span()
        .and_then(|span| text.get(..span.start))
        .map(|before| before.matches('\n').count().saturating_add(1));
    match line {
        Some(line) => format!("line {line}: {}", error.message()),
        None => error.message().to_owned(),
    }
}

/// What a recorded config shows in place of a credential.
const REDACTED: &str = "<redacted>";
/// The validator credentials a config may hold.
const CREDENTIALS: [&str; 2] = ["user", "password"];

/// The validator credentials `config` holds that are not redacted, as
/// written. Names are compared whatever their case: zainod refuses a
/// miscased one, and this must not record it.
fn exposed_credentials(config: &toml::Table) -> Vec<String> {
    config
        .iter()
        .filter(|(table, _)| table.eq_ignore_ascii_case("validator"))
        .filter_map(|(_, validator)| validator.as_table())
        .flat_map(|validator| validator.iter())
        .filter(|(key, value)| {
            CREDENTIALS
                .iter()
                .any(|credential| key.eq_ignore_ascii_case(credential))
                && value.as_str() != Some(REDACTED)
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// What follows a value on its line, when that is nothing but a comment
/// (with the space before it); else nothing. `after_equals` is the line from
/// just after its `=`, without the line ending.
fn trailing_comment(after_equals: &str) -> &str {
    let value = after_equals.trim_start();
    let mut chars = value.char_indices();
    let rest_at = match chars.next() {
        Some((_, quote @ ('"' | '\''))) => {
            let mut escaped = false;
            chars
                .find(|(_, c)| {
                    let closes = *c == quote && !escaped;
                    escaped = quote == '"' && *c == '\\' && !escaped;
                    closes
                })
                .map(|(at, c)| at.saturating_add(c.len_utf8()))
        }
        _ => value.find('#'),
    };
    let rest = rest_at.and_then(|at| value.get(at..)).unwrap_or_default();
    match rest.trim_start().starts_with('#') {
        true => rest,
        false => "",
    }
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
        let assignment = content
            .split_once('=')
            .map(|(key, value)| (key.trim(), value));
        let credential = assignment.filter(|(key, _)| {
            let path = match table.is_empty() {
                true => (*key).to_owned(),
                false => format!("{table}.{key}"),
            };
            CREDENTIALS
                .iter()
                .any(|credential| path.eq_ignore_ascii_case(&format!("validator.{credential}")))
        });
        match credential {
            Some((key, value)) => {
                let indent = line.len().saturating_sub(content.len());
                let ending = if line.ends_with('\n') { "\n" } else { "" };
                let comment = trailing_comment(value.trim_end_matches(['\n', '\r']));
                out.push_str(line.get(..indent).unwrap_or_default());
                out.push_str(&format!("{key} = \"{REDACTED}\"{comment}{ending}"));
            }
            None => out.push_str(line),
        }
    }
    let redacted: toml::Table = toml::from_str(&out).map_err(|error| {
        format!(
            "the config does not parse with its credentials redacted: {}",
            toml_fault(&out, &error)
        )
    })?;
    let exposed = exposed_credentials(&redacted);
    if !exposed.is_empty() {
        return Err(format!(
            "[validator] {} would be recorded in the clear: write it on a line of its own \
             under [validator], as `{} = \"...\"`",
            exposed.join(" and "),
            exposed.first().map_or("password", String::as_str),
        ));
    }
    Ok(out)
}

/// What zainod builds when the config has no `[indexes]` table, and the
/// base set when the table names none (`packages/zainod/src/config.rs` and
/// `IndexesConfig`'s defaults).
const SET_WITHOUT_TABLE: &str = "lightclient";
const SET_UNNAMED: &str = "empty";

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
    let mut label = match indexes {
        None => SET_WITHOUT_TABLE,
        Some(indexes) => indexes
            .get("set")
            .and_then(toml::Value::as_str)
            .unwrap_or(SET_UNNAMED),
    }
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

/// Refuses an admin address something already listens on: its answers would
/// be read as this run's zainod's.
fn admin_is_free(admin: SocketAddr) -> Result<(), String> {
    std::net::TcpListener::bind(admin)
        .map(drop)
        .map_err(|error| {
            format!(
                "[admin] listen_address {admin} cannot be bound ({error}): if another zainod is \
                 using it, stop that one or give this run another address"
            )
        })
}

/// What must hold for a run to start: every store directory empty, and the
/// admin address free.
fn preflight(config: &BenchConfig) -> Result<(), String> {
    for dir in config.store_dirs() {
        if !is_empty(dir)? {
            return Err(format!(
                "{} is not empty: a run syncs from nothing, so point the config at an empty \
                 directory",
                dir.display()
            ));
        }
    }
    admin_is_free(config.admin)
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

/// Runs `program` to its end and returns its standard output. One that has
/// not ended within [`HELPER`] is killed.
async fn output_of(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = tokio::time::timeout(HELPER, command.output())
        .await
        .map_err(|_| {
            format!(
                "`{program} {}` did not end within {} minutes",
                args.join(" "),
                HELPER.as_secs() / 60
            )
        })?
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
            let mut cargo = Command::new("cargo");
            cargo
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
                .kill_on_drop(true);
            let status = tokio::time::timeout(BUILD, cargo.status())
                .await
                .map_err(|_| {
                    failed(format!(
                        "cargo build did not end within {} hours",
                        BUILD.as_secs() / 3_600
                    ))
                })?
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
        // x86 names the model `model name`; other architectures use other keys
        cpu: ["model name", "Hardware", "cpu model", "cpu"]
            .into_iter()
            .find_map(|key| proc_field(&cpuinfo, key))
            .map(str::to_owned),
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

/// Whether a directory is named as zainod names an index's: `<index>_<format>`.
fn is_index_dir(name: &str) -> bool {
    name.rsplit_once('_').is_some_and(|(index, format)| {
        !index.is_empty() && !format.is_empty() && format.chars().all(|c| c.is_ascii_digit())
    })
}

/// `dir` resolved; `None` when it does not exist.
fn resolved(dir: &Path) -> Result<Option<PathBuf>, String> {
    match fs::canonicalize(dir) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", dir.display())),
    }
}

/// Disk used: in total, and by each index directory. Indexes are at
/// `<data dir>/<network>/indexes/<index>`, and directly under each
/// `[indexes.paths]` directory. No directory is counted twice, whether two
/// indexes share one or one lies inside another.
fn disk(config: &BenchConfig) -> Result<Disk, String> {
    let mut roots = BTreeSet::new();
    for dir in config.store_dirs() {
        roots.extend(resolved(dir)?);
    }
    let mut total_bytes = 0u64;
    for root in &roots {
        let inside_another = roots
            .iter()
            .any(|other| other != root && root.starts_with(other));
        if !inside_another {
            total_bytes = total_bytes.saturating_add(disk_bytes(root)?);
        }
    }
    let mut parents = Vec::new();
    for network in subdirs(&config.data_dir)? {
        parents.push(network.join("indexes"));
    }
    parents.extend(config.index_dirs.iter().cloned());
    let mut index_dirs = BTreeSet::new();
    for parent in parents {
        for dir in subdirs(&parent)? {
            index_dirs.extend(resolved(&dir)?);
        }
    }
    let mut by_index = BTreeMap::new();
    for dir in index_dirs {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no name", dir.display()))?;
        if !is_index_dir(&name) {
            continue;
        }
        let bytes: &mut u64 = by_index.entry(name).or_default();
        *bytes = bytes.saturating_add(disk_bytes(&dir)?);
    }
    Ok(Disk {
        total_bytes,
        by_index,
    })
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
    /// Whether zainod's admin endpoint ever answered as this run's zainod:
    /// it starts only once zainod has accepted the config.
    accepted: bool,
    /// The sync's measurements, or why the run failed.
    result: Result<Local, String>,
}

/// How zainod stopped on the tool's SIGINT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    /// It shut down and exited 0.
    Clean,
    /// The signal ended it: it was not serving yet, so not yet listening for
    /// one.
    BySignal,
}

/// How far before the run's start a process's reported start may fall and
/// still be this run's zainod: the start is reported in whole seconds, from
/// the boot time in whole seconds.
const START_SLACK_SECS: u64 = 2;

/// Refuses a `/metrics` answer from a process started before this run: it is
/// not the zainod this run started, but another on the same address.
fn is_from_this_run(process_start_unix: Option<u64>, run_started_unix: u64) -> Result<(), String> {
    let process_start = process_start_unix
        .ok_or("/metrics does not say when its process started, so it cannot be told from another zainod's")?;
    match process_start.saturating_add(START_SLACK_SECS) >= run_started_unix {
        true => Ok(()),
        false => Err(format!(
            "the process answering on the admin address started {}s before this run: it is not \
             this run's zainod",
            run_started_unix.saturating_sub(process_start)
        )),
    }
}

/// One running zainod and where its run is recorded.
struct Running<'a> {
    child: Child,
    started: Instant,
    /// The second the run started in, taken before zainod was.
    started_unix: u64,
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

    /// Reads `/metrics`, refusing an answer that is not this run's zainod's.
    async fn scrape(&self) -> Result<Sample, String> {
        let body = self.get("/metrics").await?;
        let sample = metrics::parse(&body, self.started.elapsed().as_secs_f64())
            .map_err(|error| error.to_string())?;
        is_from_this_run(sample.process_start_unix, self.started_unix)?;
        Ok(sample)
    }

    /// Reads `/metrics` and appends the sample to the run's series.
    async fn sample(&self) -> Result<Sample, String> {
        let sample = self.scrape().await?;
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

    /// Watches zainod until every answer is local, or the run fails. Every
    /// wait in it is bounded: zainod's exit, `--timeout`, an admin endpoint
    /// gone quiet, no first block, no progress.
    async fn observe(&mut self, timeout: Duration) -> Observed {
        let mut ticker = tokio::time::interval(POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ready_secs = None;
        let mut accepted = false;
        let mut last_readyz = Instant::now();
        let mut last_sample = Instant::now();
        // the indexes' heights (none before the first block) and when one
        // last moved
        let mut progress: (Vec<Option<u32>>, Instant) = (Vec::new(), Instant::now());
        let mut samples: (Option<Sample>, Option<Sample>) = (None, None);
        // whether a sample has shown the build: until one does, every poll samples
        let mut building = false;
        // when every answer was first local, and the height then
        let mut local: Option<(f64, Option<u32>)> = None;
        let mut polls = 0u64;
        let minutes = |bound: Duration| bound.as_secs() / 60;
        let cause = loop {
            let signal = tokio::select! {
                _ = ticker.tick() => None,
                _ = self.interrupt.recv() => Some("SIGINT"),
                _ = self.terminate.recv() => Some("SIGTERM"),
            };
            if let Some(signal) = signal {
                break format!("zaino-bench was interrupted ({signal}) before the sync finished");
            }
            polls = polls.saturating_add(1);
            match self.child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    break format!(
                        "zainod exited ({status}) before the sync finished; its log ends:\n{}",
                        log_tail(&self.log)
                    )
                }
                Err(error) => break format!("zainod's process could not be watched: {error}"),
            }
            if self.started.elapsed() >= timeout {
                break format!("the sync did not finish within --timeout ({timeout:?})");
            }
            // no answer is trusted until `/metrics` shows it is this run's
            // zainod answering, not another on the same address
            if !accepted {
                match self.scrape().await {
                    Ok(_) => accepted = true,
                    Err(error) if last_readyz.elapsed() >= ADMIN_SILENCE => {
                        break format!(
                            "zainod's admin endpoint has not answered as this run's zainod for \
                             {} minutes: {error}",
                            minutes(ADMIN_SILENCE)
                        );
                    }
                    Err(_) => {
                        status_line(&format!(
                            "waiting for zainod's admin endpoint  {}",
                            report::duration(self.started.elapsed().as_secs_f64())
                        ));
                        continue;
                    }
                }
            }
            let readyz = match self
                .get("/readyz")
                .await
                .and_then(|body| Readyz::parse(&body))
            {
                Ok(readyz) => {
                    last_readyz = Instant::now();
                    readyz
                }
                Err(error) if last_readyz.elapsed() >= ADMIN_SILENCE => {
                    break format!(
                        "zainod's /readyz has not answered for {} minutes: {error}",
                        minutes(ADMIN_SILENCE)
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
                break critical;
            }
            let elapsed = self.started.elapsed().as_secs_f64();
            if readyz.ready && ready_secs.is_none() {
                ready_secs = Some(elapsed);
            }
            if readyz.local && local.is_none() {
                local = Some((elapsed, readyz.watermark()));
            }
            // a read that fails is asked again at the next poll, within the bound
            if local.is_some() || !building || polls.is_multiple_of(POLLS_PER_SAMPLE) {
                match self.sample().await {
                    Ok(sample) => {
                        last_sample = Instant::now();
                        building = building || !sample.is_before_build();
                        if let Some((sync_secs, height)) = local {
                            let result = self.peak_memory().map(|peak_memory_bytes| Local {
                                sync_secs,
                                height,
                                last: sample,
                                peak_memory_bytes,
                            });
                            return Observed {
                                ready_secs,
                                accepted,
                                result,
                            };
                        }
                        samples = (samples.1.take(), Some(sample));
                    }
                    Err(error) if last_sample.elapsed() >= ADMIN_SILENCE => {
                        break format!(
                            "zainod's /metrics could not be read for {} minutes: {error}",
                            minutes(ADMIN_SILENCE)
                        );
                    }
                    Err(_) => {}
                }
            }
            if local.is_some() {
                // the sync is timed: only its last sample is still owed
                continue;
            }
            // `/readyz` lists the indexes only once zainod serves; until then
            // the last `/metrics` sample has their heights
            let mut heights = readyz.heights();
            if heights.is_empty() {
                if let Some(sample) = &samples.1 {
                    heights = sample.heights.values().copied().map(Some).collect();
                }
            }
            if progress.0 != heights {
                progress = (heights, Instant::now());
            } else if progress.0.is_empty() && progress.1.elapsed() >= FIRST_BLOCK {
                break format!(
                    "zainod built no block in its first {} minutes",
                    minutes(FIRST_BLOCK)
                );
            } else if !progress.0.is_empty() && progress.1.elapsed() >= STALL {
                break format!("no index has advanced for {} minutes", minutes(STALL));
            }
            status_line(&progress_line(&readyz, &samples, elapsed));
        };
        // a sync that was timed but could not be measured says both
        let cause = match local {
            Some((sync_secs, _)) => format!(
                "every answer was local after {}, but the run's last measurements could not be \
                 taken: {cause}",
                report::polled(sync_secs)
            ),
            None => cause,
        };
        Observed {
            ready_secs,
            accepted,
            result: Err(cause),
        }
    }

    /// Kills zainod outright; what came of it, for the cause.
    async fn kill(&mut self) -> String {
        match self.child.kill().await {
            Ok(()) => "it was killed".to_owned(),
            Err(error) => format!("and it could not be killed: {error}"),
        }
    }

    /// Stops zainod with SIGINT: it must exit within [`SHUTDOWN`]; one that
    /// does not is killed, as is one still shutting down when this tool is
    /// interrupted again.
    async fn stop(&mut self) -> Result<Stopped, String> {
        let Some(pid) = self.child.id() else {
            return match self.child.wait().await {
                Ok(status) => Err(format!("zainod had already exited ({status})")),
                Err(error) => Err(format!("zainod had already exited: {error}")),
            };
        };
        // a zainod that cannot be signalled is still never left running
        if let Err(error) = output_of("kill", &["-INT", &pid.to_string()], None).await {
            return Err(format!(
                "zainod could not be sent SIGINT ({error}); {}",
                self.kill().await
            ));
        }
        let waited = tokio::select! {
            waited = tokio::time::timeout(SHUTDOWN, self.child.wait()) => Ok(waited),
            _ = self.interrupt.recv() => Err("SIGINT"),
            _ = self.terminate.recv() => Err("SIGTERM"),
        };
        match waited {
            Ok(Ok(Ok(status))) if status.success() => Ok(Stopped::Clean),
            Ok(Ok(Ok(status)))
                if status.signal() == Some(SignalKind::interrupt().as_raw_value()) =>
            {
                Ok(Stopped::BySignal)
            }
            Ok(Ok(Ok(status))) => Err(format!(
                "zainod did not shut down cleanly ({status}); its log ends:\n{}",
                log_tail(&self.log)
            )),
            Ok(Ok(Err(error))) => Err(format!("waiting for zainod to exit: {error}")),
            Ok(Err(_)) => Err(format!(
                "zainod did not exit within {} minutes of SIGINT; {}",
                SHUTDOWN.as_secs() / 60,
                self.kill().await
            )),
            Err(signal) => Err(format!(
                "zaino-bench was interrupted ({signal}) while zainod was shutting down; {}",
                self.kill().await
            )),
        }
    }
}

/// Rewrites the terminal's status line; nothing when not on a terminal.
fn status_line(line: &str) {
    if std::io::stderr().is_terminal() {
        crate::print_err(&format!("\r\x1b[2K{line}"));
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
/// when the sync completed with nothing to warn of; `Ok(false)` when the run
/// failed, or completed with warnings, and was recorded and printed as such.
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
    // checked before the build, to refuse at once, and again after it, as
    // building zainod can take long enough for either to change
    preflight(&config).map_err(config_error)?;
    let zainod = zainod(prebuilt).await?;
    let host = host(&config.data_dir)?;
    preflight(&config).map_err(config_error)?;

    // a run is named by the second it starts in; one already taken (another
    // run started this second) is waited out
    let launch_error = |cause: String| RunError::new(Step::Launch, cause);
    let mut attempts = 0u32;
    let (started_unix, run_id, run_dir) = loop {
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| launch_error(format!("the clock: {error}")))?
            .as_secs();
        let run_id = format!("run-{started_unix}");
        let created = results
            .create_run_dir(&run_id)
            .map_err(|error| RunError::new(Step::Record, error))?;
        if let Some(run_dir) = created {
            break (started_unix, run_id, run_dir);
        }
        attempts = attempts.saturating_add(1);
        if attempts >= RUN_ID_ATTEMPTS {
            return Err(RunError::new(
                Step::Record,
                format!(
                    "{run_id} is taken, like the id of each second tried before it \
                     ({attempts} tries): other runs keep starting"
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let log = run_dir.join("zainod.log");
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
    // `HOME` and the XDG directories stay, as zainod's default paths come
    // from them; the log goes to a file, so without colour codes.
    //
    // Started through `setpriv --pdeathsig`: the kernel then sends zainod
    // SIGTERM if this tool dies without stopping it (killed outright), so
    // zainod is never left running on its own.
    let mut command = Command::new(PARENT_DEATH_LAUNCHER);
    command
        .args(["--pdeathsig", "TERM", "--"])
        .arg(&zainod.binary)
        .arg("start")
        .arg("--config")
        .arg(config_path)
        .env_clear()
        .env("ZAINOLOG_COLOR", "0")
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_errors)
        .kill_on_drop(true);
    for kept in KEPT_ENVIRONMENT {
        if let Some(value) = std::env::var_os(kept) {
            command.env(kept, value);
        }
    }
    crate::print_err(&format!(
        "zaino-bench {run_id}: {} on indexes `{}`; log {}\n",
        zainod.binary.display(),
        config.indexes,
        log.display()
    ));
    // from here a signal to this tool stops zainod and records the run
    let on = |kind: SignalKind| {
        signal(kind).map_err(|error| launch_error(format!("listening for signals: {error}")))
    };
    let (interrupt, terminate) = (on(SignalKind::interrupt())?, on(SignalKind::terminate())?);
    let child = command.spawn().map_err(|error| {
        launch_error(format!(
            "`{PARENT_DEATH_LAUNCHER}` (util-linux), which starts {} so that it stops if this \
             tool is killed: {error}",
            zainod.binary.display()
        ))
    })?;
    let mut running = Running {
        interrupt,
        terminate,
        child,
        started: Instant::now(),
        started_unix,
        config: &config,
        results,
        run_id: &run_id,
        log,
        http,
    };

    let observed = running.observe(timeout).await;
    if std::io::stderr().is_terminal() {
        crate::print_err("\n");
    }
    let had_exited = running.child.id().is_none();
    let stopped = running.stop().await;
    let outcome = match observed.result {
        // a sync that completed stays a completed sync: trouble stopping
        // zainod or measuring the disk is recorded with it, never instead of it
        Ok(local) => {
            let shutdown = match stopped {
                Ok(Stopped::Clean) => Shutdown::Clean,
                Ok(Stopped::BySignal) => Shutdown::Failed {
                    cause: "the SIGINT ended zainod instead of shutting it down".to_owned(),
                },
                Err(cause) => Shutdown::Failed { cause },
            };
            let disk = match disk(&config) {
                Ok(disk) => DiskUse::Measured(disk),
                Err(cause) => DiskUse::Unmeasured { cause },
            };
            let last = local.last;
            match (last.cpu_secs, last.restarts, last.build_failures) {
                (Some(cpu_secs), Some(restarts), Some(build_failures)) => Outcome::Synced(Synced {
                    sync_secs: local.sync_secs,
                    height: local.height,
                    peak_memory_bytes: local.peak_memory_bytes,
                    cpu_secs,
                    disk,
                    stages: last.stages,
                    restarts,
                    build_failures,
                    shutdown,
                }),
                _ => Outcome::Failed {
                    cause: format!(
                        "every answer was local after {}, but zainod's /metrics does not report \
                         its CPU time, restarts and failed builds (CPU {:?}, restarts {:?}, \
                         failed builds {:?})",
                        report::polled(local.sync_secs),
                        last.cpu_secs,
                        last.restarts,
                        last.build_failures
                    ),
                },
            }
        }
        Err(cause) => match stopped {
            // zainod not yet serving has no handler for the signal, which then
            // ends it: stopped either way
            Ok(Stopped::Clean | Stopped::BySignal) => Outcome::Failed { cause },
            // the exit is the failure already reported
            Err(_) if had_exited => Outcome::Failed { cause },
            Err(stop) => Outcome::Failed {
                cause: format!("{cause}\n{stop}"),
            },
        },
    };
    let record = Record {
        run_id,
        started_unix,
        indexes: config.indexes.clone(),
        config: observed.accepted.then(|| config.recorded.clone()),
        zainod,
        host,
        ready_secs: observed.ready_secs,
        outcome,
    };
    results
        .append_record(&record)
        .map_err(|error| RunError::new(Step::Record, error))?;
    // the run is recorded: a series that cannot be read back costs the
    // summary its band analysis, and says so
    let mut problems = Vec::new();
    let series = match results.series(&record.run_id) {
        Ok(mut series) => {
            problems.append(&mut series.damaged);
            series.values
        }
        Err(error) => {
            problems.push(format!("the run's series could not be read back: {error}"));
            Vec::new()
        }
    };
    crate::print_out(&report::run_summary(&record, &series, &problems))
        .map_err(|cause| RunError::new(Step::Summary, cause))?;
    Ok(match &record.outcome {
        Outcome::Synced(synced) => synced.warnings().is_empty() && problems.is_empty(),
        Outcome::Failed { .. } => false,
    })
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
  user = \"<redacted>\"   # the rpc user
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

    /// zainod refuses a miscased key; the copy recorded must not hold it.
    #[test]
    fn a_miscased_credential_is_redacted_too() {
        for config in [
            "[validator]\nPassword = \"hunter2\"\nUSER = \"alice\"\n",
            "[Validator]\npassword = \"hunter2\"\nuser = \"alice\"\n",
            "Validator.Password = \"hunter2\"\n",
        ] {
            let recorded = redact_credentials(config).expect("redacts");
            assert!(!recorded.contains("hunter2"), "{recorded}");
            assert!(!recorded.contains("alice"), "{recorded}");
        }
    }

    /// A config that does not parse is refused by line and message: the
    /// parser's excerpt of the file, which can be a credential's line, is
    /// never shown.
    #[test]
    fn a_config_that_does_not_parse_is_refused_without_quoting_it() {
        for config in [
            "[validator]\npassword = \"hunter2\nuser = \"alice\"\n",
            "[validator]\npassword = \"hunter2\" trailing\n",
            "[validator]\npassword = hunter2\n",
        ] {
            let refused = BenchConfig::parse(config).expect_err("does not parse");
            assert!(!refused.contains("hunter2"), "{refused}");
            assert!(refused.contains("line "), "{refused}");
        }
    }

    #[test]
    fn only_a_comment_is_kept_after_a_redacted_value() {
        assert_eq!(trailing_comment(" \"a\"  # why"), "  # why");
        assert_eq!(trailing_comment(" \"a # not a comment\""), "");
        assert_eq!(
            trailing_comment(" \"a\\\" # still inside\" # out"),
            " # out"
        );
        assert_eq!(trailing_comment(" 'a\\' # out"), " # out");
        assert_eq!(trailing_comment(" \"a\""), "");
        assert_eq!(trailing_comment(" \"\"\"hunter2\"\"\""), "");
        assert_eq!(trailing_comment(" 7 # seven"), "# seven");
        assert_eq!(trailing_comment(""), "");
        // what is not a comment is never copied into the record
        let recorded =
            redact_credentials("[validator]\npassword = \"\"\"hunter2\"\"\"\n").expect("redacts");
        assert!(!recorded.contains("hunter2"), "{recorded}");
    }

    #[test]
    fn an_answer_from_a_process_older_than_the_run_is_refused() {
        let run_started = 1_795_000_100;
        is_from_this_run(Some(run_started), run_started).expect("started with the run");
        is_from_this_run(Some(run_started + 3), run_started).expect("started after it");
        // the start is reported in whole seconds, from a truncated boot time
        is_from_this_run(Some(run_started - START_SLACK_SECS), run_started).expect("in slack");
        let older = is_from_this_run(Some(run_started - 60), run_started).expect_err("older");
        assert!(older.contains("60s before this run"), "{older}");
        assert!(is_from_this_run(None, run_started).is_err());
    }

    #[test]
    fn an_admin_address_in_use_is_refused() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
        let address = taken.local_addr().expect("an address");
        let refused = admin_is_free(address).expect_err("in use");
        assert!(refused.contains(&address.to_string()), "{refused}");
        drop(taken);
        admin_is_free(address).expect("free again");
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
    }

    #[test]
    fn the_indexes_label_names_what_zainod_builds() {
        let label = |indexes: &str| {
            let config = format!(
                "[persistence]\npath = \"/data\"\n[admin]\nlisten_address = \
                 \"127.0.0.1:9998\"\n{indexes}"
            );
            BenchConfig::parse(&config).expect("parses").indexes
        };
        assert_eq!(label(""), "lightclient", "zainod's set without [indexes]");
        assert_eq!(label("[indexes]\nset = \"full\"\n"), "full");
        assert_eq!(
            label("[indexes]\nenable = [\"spent\"]\n"),
            "empty +spent",
            "an [indexes] table that names no set starts from none"
        );
        assert_eq!(
            label("[indexes]\nset = \"zcash_rpc\"\ndisable = [\"spent\"]\n"),
            "zcash_rpc -spent"
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
            (moved.join("not an index"), "a", 40),
            (root.join("store/inside/spent_1"), "a", 9),
            (root.join("store/regtest"), "PRIMARY", 3),
        ] {
            fs::create_dir_all(&dir).expect("created");
            fs::write(dir.join(file), vec![0u8; bytes]).expect("written");
        }
        let mut config = BenchConfig {
            recorded: String::new(),
            data_dir: root.join("store"),
            index_dirs: vec![moved.clone(), root.join("never-created")],
            admin: SocketAddr::from(([127, 0, 0, 1], 1)),
            indexes: String::new(),
        };
        assert!(!is_empty(&config.data_dir).expect("read"));
        assert!(is_empty(&root.join("never-created")).expect("absent is empty"));
        let measured = disk(&config).expect("measured");
        assert_eq!(measured.total_bytes, 174, "every file, once");
        let by_index = BTreeMap::from([
            ("compact_block_1".to_owned(), 100),
            ("headers_2".to_owned(), 15),
            ("txids_1".to_owned(), 7),
        ]);
        assert_eq!(measured.by_index, by_index, "index directories only");

        // one directory named for two indexes, and one inside the data
        // directory: nothing is counted twice
        config.index_dirs = vec![moved.clone(), moved, root.join("store/inside")];
        let measured = disk(&config).expect("measured");
        assert_eq!(measured.total_bytes, 174);
        let mut with_inside = by_index;
        with_inside.insert("spent_1".to_owned(), 9);
        assert_eq!(measured.by_index, with_inside);
        fs::remove_dir_all(&root).expect("the scratch directory is removed");
    }

    #[test]
    fn index_directories_are_named_index_then_format() {
        assert!(is_index_dir("headers_2") && is_index_dir("txid_location_10"));
        for other in ["regtest", "indexes", "headers_", "_2", "headers_v2", ""] {
            assert!(!is_index_dir(other), "{other}");
        }
    }
}
