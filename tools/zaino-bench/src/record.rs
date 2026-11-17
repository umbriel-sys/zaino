//! One JSON record per run, appended under `bench-results/`. Working notes
//! only: the published figures come from the final validation (Stage 8).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::recorder::Snapshot;
use crate::StoreKind;

#[derive(serde::Serialize)]
pub struct Record {
    pub command: &'static str,
    pub recorded_at_unix: u64,
    pub host: Host,
    pub store: Option<StoreKind>,
    pub set: Option<String>,
    /// `--lsm-threads` where given (a build's pool size per index).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsm_threads: Option<usize>,
    /// `--lsm-write-buffer-mib` on a build.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsm_write_buffer_mib: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<crate::build::Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<crate::verify::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<crate::compare::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<crate::read::Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Snapshot>,
}

#[derive(serde::Serialize)]
pub struct Host {
    pub cpus: usize,
    pub kernel: String,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host() -> Host {
    Host {
        cpus: std::thread::available_parallelism().map_or(0, |n| n.get()),
        kernel: std::fs::read_to_string("/proc/version")
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

impl Record {
    fn base(command: &'static str, store: Option<StoreKind>, set: Option<&str>) -> Self {
        Self {
            command,
            recorded_at_unix: now(),
            host: host(),
            store,
            set: set.map(str::to_owned),
            lsm_threads: None,
            lsm_write_buffer_mib: None,
            build: None,
            verify: None,
            compare: None,
            read: None,
            metrics: None,
        }
    }

    pub fn build(
        store: StoreKind,
        set: &str,
        _to: u32,
        outcome: &crate::build::Outcome,
        metrics: &Snapshot,
    ) -> Self {
        Self {
            build: Some(outcome.clone()),
            metrics: Some(metrics.clone()),
            ..Self::base("build", Some(store), Some(set))
        }
    }

    pub fn verify(store: StoreKind, set: &str, report: &crate::verify::Report) -> Self {
        Self {
            verify: Some(report.clone()),
            ..Self::base("verify", Some(store), Some(set))
        }
    }

    pub fn compare(_from: u32, _to: u32, report: &crate::compare::Report) -> Self {
        Self {
            compare: Some(report.clone()),
            ..Self::base("compare", None, None)
        }
    }

    pub fn read(
        store: StoreKind,
        set: &str,
        _from: u32,
        _to: u32,
        outcome: &crate::read::Outcome,
        metrics: &Snapshot,
    ) -> Self {
        Self {
            read: Some(outcome.clone()),
            metrics: Some(metrics.clone()),
            ..Self::base("read", Some(store), Some(set))
        }
    }
}

/// The results directory.
pub struct Results {
    dir: PathBuf,
}

impl Results {
    pub fn new(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("creating {}: {error}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Appends `record` as one line of `<command>.jsonl`.
    pub fn append(&self, record: &Record) -> Result<(), String> {
        let path = self.dir.join(format!("{}.jsonl", record.command));
        let line = serde_json::to_string(record).map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("opening {}: {error}", path.display()))?;
        writeln!(file, "{line}").map_err(|error| error.to_string())?;
        println!("recorded in {}", path.display());
        Ok(())
    }
}
