//! What a run leaves behind: one record line in `runs.jsonl`, and the run's
//! own directory holding its metric samples and zainod's log.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A stage timer as zainod reports it: seconds spent and blocks timed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Timer {
    pub(crate) secs: f64,
    pub(crate) blocks: u64,
}

/// The stage timers zainod keeps for each index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexStages {
    pub(crate) queue_wait: Timer,
    pub(crate) writer_wait: Timer,
    pub(crate) fold: Timer,
    pub(crate) commit: Timer,
}

/// Every stage timer at one moment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Stages {
    pub(crate) fetch: Timer,
    pub(crate) resolve: Timer,
    pub(crate) indexes: BTreeMap<String, IndexStages>,
}

/// One reading of zainod's `/metrics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sample {
    /// Seconds since zainod was started.
    pub(crate) elapsed_secs: f64,
    /// The lowest index's height; `None` before the first block.
    pub(crate) watermark: Option<u32>,
    /// The height the build runs to; `None` before the build starts.
    pub(crate) target: Option<u32>,
    /// Each index's height, once it holds a block.
    pub(crate) heights: BTreeMap<String, u32>,
    pub(crate) stages: Stages,
    /// CPU seconds zainod has used.
    pub(crate) cpu_secs: Option<f64>,
}

/// The zainod that ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Zainod {
    pub(crate) binary: PathBuf,
    pub(crate) sha256: String,
    /// The checkout it was built from; `None` for a prebuilt binary.
    pub(crate) build: Option<Build>,
}

/// The checkout a zainod was built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Build {
    pub(crate) commit: String,
    pub(crate) dirty: bool,
}

/// The machine a run was made on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Host {
    pub(crate) cpu: String,
    pub(crate) cores: usize,
    pub(crate) ram_bytes: u64,
    pub(crate) kernel: String,
    /// The filesystem type under the data directory.
    pub(crate) filesystem: String,
}

/// What a completed sync measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Synced {
    /// Seconds from start until every answer was local.
    pub(crate) sync_secs: f64,
    /// The height the store was built to; `None` when the config builds no index.
    pub(crate) height: Option<u32>,
    pub(crate) peak_memory_bytes: u64,
    pub(crate) cpu_secs: f64,
    pub(crate) disk_total_bytes: u64,
    pub(crate) disk_bytes: BTreeMap<String, u64>,
    pub(crate) stages: Stages,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Synced(Synced),
    Failed { cause: String },
}

/// One run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) run_id: String,
    /// Seconds since the Unix epoch at zainod's start.
    pub(crate) started_unix: u64,
    /// The `[indexes]` the config names, for display.
    pub(crate) indexes: String,
    /// The config file that ran, verbatim but for the validator's `user` and
    /// `password`, which are redacted.
    pub(crate) config: String,
    pub(crate) zainod: Zainod,
    pub(crate) host: Host,
    /// Seconds from start until zainod was serving; `None` if it never was.
    pub(crate) ready_secs: Option<f64>,
    pub(crate) outcome: Outcome,
}

/// A record or series file could not be written or read.
#[derive(Debug)]
pub(crate) struct RecordError {
    path: PathBuf,
    cause: String,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.cause)
    }
}

impl RecordError {
    fn at(path: &Path, cause: impl fmt::Display) -> Self {
        Self {
            path: path.to_owned(),
            cause: cause.to_string(),
        }
    }
}

/// Where runs are recorded.
#[derive(Debug, Clone)]
pub(crate) struct Results {
    root: PathBuf,
}

impl Results {
    /// The tool's own `bench-results/` (git-ignored).
    pub(crate) fn default_dir() -> Self {
        Self::at(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench-results"))
    }

    fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn records_path(&self) -> PathBuf {
        self.root.join("runs.jsonl")
    }

    /// The directory of `run_id`'s series and log.
    pub(crate) fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join("runs").join(run_id)
    }

    fn series_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("series.jsonl")
    }

    /// Creates `run_id`'s directory; a run that already has one is refused.
    pub(crate) fn create_run_dir(&self, run_id: &str) -> Result<PathBuf, RecordError> {
        let dir = self.run_dir(run_id);
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|error| RecordError::at(parent, error))?;
        }
        fs::create_dir(&dir).map_err(|error| RecordError::at(&dir, error))?;
        Ok(dir)
    }

    pub(crate) fn append_record(&self, record: &Record) -> Result<(), RecordError> {
        append_line(&self.records_path(), record)
    }

    pub(crate) fn append_sample(&self, run_id: &str, sample: &Sample) -> Result<(), RecordError> {
        append_line(&self.series_path(run_id), sample)
    }

    /// Every recorded run, oldest first; none when nothing was recorded yet.
    pub(crate) fn records(&self) -> Result<Vec<Record>, RecordError> {
        read_lines(&self.records_path())
    }

    /// `run_id`'s samples, in order; none when the run took no sample.
    pub(crate) fn series(&self, run_id: &str) -> Result<Vec<Sample>, RecordError> {
        read_lines(&self.series_path(run_id))
    }
}

fn append_line<T: Serialize>(path: &Path, value: &T) -> Result<(), RecordError> {
    let mut line = serde_json::to_string(value).map_err(|error| RecordError::at(path, error))?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| RecordError::at(path, error))?;
    file.write_all(line.as_bytes())
        .map_err(|error| RecordError::at(path, error))
}

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, RecordError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(RecordError::at(path, error)),
    };
    let mut values = Vec::new();
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| RecordError::at(path, error))?;
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(&line).map_err(|error| {
            RecordError::at(path, format!("line {}: {error}", number.saturating_add(1)))
        })?;
        values.push(value);
    }
    Ok(values)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synced record for tests, built to `height` in `sync_secs`.
    pub(crate) fn synced(run_id: &str, indexes: &str, sync_secs: f64, height: u32) -> Record {
        let mut stages = Stages {
            fetch: Timer {
                secs: 40.0,
                blocks: 1_000,
            },
            resolve: Timer {
                secs: 10.0,
                blocks: 1_000,
            },
            indexes: BTreeMap::new(),
        };
        stages.indexes.insert(
            "headers".to_owned(),
            IndexStages {
                queue_wait: Timer {
                    secs: 1.0,
                    blocks: 1_000,
                },
                writer_wait: Timer {
                    secs: 2.0,
                    blocks: 1_000,
                },
                fold: Timer {
                    secs: 30.0,
                    blocks: 1_000,
                },
                commit: Timer {
                    secs: 17.0,
                    blocks: 1_000,
                },
            },
        );
        Record {
            run_id: run_id.to_owned(),
            started_unix: 1_795_000_000,
            indexes: indexes.to_owned(),
            config: "network = \"Regtest\"\n".to_owned(),
            zainod: Zainod {
                binary: PathBuf::from("/tmp/zainod"),
                sha256: "ab".repeat(32),
                build: Some(Build {
                    commit: "7c798f426".to_owned(),
                    dirty: false,
                }),
            },
            host: Host {
                cpu: "Test CPU".to_owned(),
                cores: 8,
                ram_bytes: 32 << 30,
                kernel: "6.1.0".to_owned(),
                filesystem: "ext4".to_owned(),
            },
            ready_secs: Some(1.5),
            outcome: Outcome::Synced(Synced {
                sync_secs,
                height: Some(height),
                peak_memory_bytes: 2 << 30,
                cpu_secs: 99.5,
                disk_total_bytes: 3_000,
                disk_bytes: BTreeMap::from([("headers_2".to_owned(), 3_000)]),
                stages,
            }),
        }
    }

    /// A scratch results directory, removed when dropped.
    struct Scratch(Results);

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("zaino-bench-test-{name}-{}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).expect("the old scratch directory is removed");
            }
            Self(Results::at(root))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if self.0.root().exists() {
                fs::remove_dir_all(self.0.root()).expect("the scratch directory is removed");
            }
        }
    }

    #[test]
    fn records_and_samples_round_trip() {
        let scratch = Scratch::new("round-trip");
        let results = &scratch.0;
        assert_eq!(results.records().expect("no file yet"), Vec::new());

        let synced = synced("run-1", "full", 120.0, 1_200);
        let failed = Record {
            run_id: "run-2".to_owned(),
            ready_secs: None,
            outcome: Outcome::Failed {
                cause: "zainod exited".to_owned(),
            },
            ..synced.clone()
        };
        results.create_run_dir("run-1").expect("created");
        results.append_record(&synced).expect("appended");
        results.append_record(&failed).expect("appended");
        assert_eq!(results.records().expect("read"), vec![synced, failed]);

        let sample = Sample {
            elapsed_secs: 5.0,
            watermark: Some(10),
            target: Some(1_200),
            heights: BTreeMap::from([("headers".to_owned(), 10)]),
            stages: Stages::default(),
            cpu_secs: Some(1.25),
        };
        results.append_sample("run-1", &sample).expect("appended");
        results.append_sample("run-1", &sample).expect("appended");
        assert_eq!(
            results.series("run-1").expect("read"),
            vec![sample.clone(), sample]
        );
        assert_eq!(results.series("run-2").expect("no series"), Vec::new());
    }

    #[test]
    fn a_run_directory_is_never_reused() {
        let scratch = Scratch::new("reuse");
        scratch.0.create_run_dir("run-1").expect("created");
        assert!(scratch.0.create_run_dir("run-1").is_err());
    }

    #[test]
    fn a_damaged_record_line_is_an_error_naming_the_line() {
        let scratch = Scratch::new("damaged");
        fs::create_dir_all(scratch.0.root()).expect("created");
        fs::write(scratch.0.root().join("runs.jsonl"), "{not json}\n").expect("written");
        let error = scratch.0.records().expect_err("damaged").to_string();
        assert!(error.contains("line 1"), "{error}");
    }
}
