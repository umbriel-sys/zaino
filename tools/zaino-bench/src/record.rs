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
    /// Times zainod has restarted itself after losing its validator.
    pub(crate) restarts: Option<u64>,
    /// Store builds that failed (each is retried).
    pub(crate) build_failures: Option<u64>,
    /// When the process that answered started, in seconds since the Unix
    /// epoch: what tells this run's zainod from another on the same address.
    pub(crate) process_start_unix: Option<u64>,
}

impl Sample {
    /// Whether the sample was taken before the store built its first block.
    pub(crate) fn is_before_build(&self) -> bool {
        self.target.is_none() && self.watermark.is_none() && self.heights.is_empty()
    }
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
    /// The CPU model; `None` on a machine whose `/proc/cpuinfo` names none.
    pub(crate) cpu: Option<String>,
    pub(crate) cores: usize,
    pub(crate) ram_bytes: u64,
    pub(crate) kernel: String,
    /// The filesystem type under the data directory.
    pub(crate) filesystem: String,
}

/// Disk used by a store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Disk {
    pub(crate) total_bytes: u64,
    /// Per index directory (`<index>_<format>`).
    pub(crate) by_index: BTreeMap<String, u64>,
}

/// The disk a synced store uses, or why it could not be measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiskUse {
    Measured(Disk),
    Unmeasured { cause: String },
}

/// How zainod stopped once the sync was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Shutdown {
    /// It exited 0 on SIGINT.
    Clean,
    Failed {
        cause: String,
    },
}

/// What a completed sync measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Synced {
    /// Seconds from start until the first once-a-second poll at which every
    /// answer was local.
    pub(crate) sync_secs: f64,
    /// The height the store was built to (the chain head's floor, below the
    /// tip); `None` when the config builds no index.
    pub(crate) height: Option<u32>,
    pub(crate) peak_memory_bytes: u64,
    pub(crate) cpu_secs: f64,
    pub(crate) disk: DiskUse,
    pub(crate) stages: Stages,
    /// Times zainod restarted itself during the sync.
    pub(crate) restarts: u64,
    /// Store builds that failed and were retried during the sync.
    pub(crate) build_failures: u64,
    pub(crate) shutdown: Shutdown,
}

impl Synced {
    /// What went wrong around a sync that still completed, each to be shown
    /// wherever the run is.
    pub(crate) fn warnings(&self) -> Vec<String> {
        let times = |count: u64| if count == 1 { "once" } else { "more than once" };
        let mut warnings = Vec::new();
        if self.restarts > 0 {
            warnings.push(format!(
                "zainod restarted itself {} ({}) during the sync: the sync time includes it",
                times(self.restarts),
                self.restarts
            ));
        }
        if self.build_failures > 0 {
            warnings.push(format!(
                "a store build failed and was retried {} ({}): the sync time includes the waits",
                times(self.build_failures),
                self.build_failures
            ));
        }
        if let Shutdown::Failed { cause } = &self.shutdown {
            warnings.push(format!(
                "zainod did not shut down cleanly after the sync, so the disk figures may be of \
                 an unflushed store: {cause}"
            ));
        }
        if let DiskUse::Unmeasured { cause } = &self.disk {
            warnings.push(format!("disk use could not be measured: {cause}"));
        }
        warnings
    }
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
    /// `password`, which are redacted. `None` when zainod never got as far
    /// as accepting it: a rejected config may hold a credential under a key
    /// this tool cannot recognise.
    pub(crate) config: Option<String>,
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
    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join("runs").join(run_id)
    }

    fn series_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("series.jsonl")
    }

    /// Creates `run_id`'s directory; `None` when another run already has
    /// that id, whose directory is left alone.
    pub(crate) fn create_run_dir(&self, run_id: &str) -> Result<Option<PathBuf>, RecordError> {
        let dir = self.run_dir(run_id);
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|error| RecordError::at(parent, error))?;
        }
        match fs::create_dir(&dir) {
            Ok(()) => Ok(Some(dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(RecordError::at(&dir, error)),
        }
    }

    pub(crate) fn append_record(&self, record: &Record) -> Result<(), RecordError> {
        append_line(&self.records_path(), record)
    }

    pub(crate) fn append_sample(&self, run_id: &str, sample: &Sample) -> Result<(), RecordError> {
        append_line(&self.series_path(run_id), sample)
    }

    /// Every recorded run, oldest first; none when nothing was recorded yet.
    pub(crate) fn records(&self) -> Result<Lines<Record>, RecordError> {
        read_lines(&self.records_path())
    }

    /// `run_id`'s samples, in order; none when the run took no sample.
    pub(crate) fn series(&self, run_id: &str) -> Result<Lines<Sample>, RecordError> {
        read_lines(&self.series_path(run_id))
    }
}

/// The lines of a record or series file: those read, and each that could not
/// be, named with its file, line number and fault. One damaged line never
/// hides the rest, and is never dropped unsaid.
#[derive(Debug, PartialEq)]
pub(crate) struct Lines<T> {
    pub(crate) values: Vec<T>,
    pub(crate) damaged: Vec<String>,
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

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Lines<T>, RecordError> {
    let mut lines = Lines {
        values: Vec::new(),
        damaged: Vec::new(),
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(lines),
        Err(error) => return Err(RecordError::at(path, error)),
    };
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| RecordError::at(path, error))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(value) => lines.values.push(value),
            Err(error) => lines.damaged.push(format!(
                "{} line {} is damaged and was left out: {error}",
                path.display(),
                number.saturating_add(1)
            )),
        }
    }
    Ok(lines)
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
            config: Some("network = \"Regtest\"\n".to_owned()),
            zainod: Zainod {
                binary: PathBuf::from("/tmp/zainod"),
                sha256: "ab".repeat(32),
                build: Some(Build {
                    commit: "7c798f426".to_owned(),
                    dirty: false,
                }),
            },
            host: Host {
                cpu: Some("Test CPU".to_owned()),
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
                disk: DiskUse::Measured(Disk {
                    total_bytes: 3_000,
                    by_index: BTreeMap::from([("headers_2".to_owned(), 3_000)]),
                }),
                stages,
                restarts: 0,
                build_failures: 0,
                shutdown: Shutdown::Clean,
            }),
        }
    }

    /// The `Synced` of a record built by [`synced`].
    pub(crate) fn synced_of(record: &mut Record) -> &mut Synced {
        match &mut record.outcome {
            Outcome::Synced(synced) => synced,
            Outcome::Failed { cause } => panic!("not synced: {cause}"),
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
        let none = results.records().expect("no file yet");
        assert!(none.values.is_empty() && none.damaged.is_empty());

        let synced = synced("run-1", "full", 120.0, 1_200);
        let failed = Record {
            run_id: "run-2".to_owned(),
            ready_secs: None,
            config: None,
            outcome: Outcome::Failed {
                cause: "zainod exited".to_owned(),
            },
            ..synced.clone()
        };
        let mut warned = synced.clone();
        let measured = synced_of(&mut warned);
        measured.disk = DiskUse::Unmeasured {
            cause: "permission denied".to_owned(),
        };
        measured.shutdown = Shutdown::Failed {
            cause: "killed".to_owned(),
        };
        assert!(results.create_run_dir("run-1").expect("created").is_some());
        for record in [&synced, &failed, &warned] {
            results.append_record(record).expect("appended");
        }
        let read = results.records().expect("read");
        assert_eq!(read.values, vec![synced, failed, warned]);
        assert!(read.damaged.is_empty());

        let sample = Sample {
            elapsed_secs: 5.0,
            watermark: Some(10),
            target: Some(1_200),
            heights: BTreeMap::from([("headers".to_owned(), 10)]),
            stages: Stages::default(),
            cpu_secs: Some(1.25),
            restarts: Some(0),
            build_failures: Some(1),
            process_start_unix: Some(1_795_000_000),
        };
        results.append_sample("run-1", &sample).expect("appended");
        results.append_sample("run-1", &sample).expect("appended");
        assert_eq!(
            results.series("run-1").expect("read").values,
            vec![sample.clone(), sample]
        );
        assert!(results
            .series("run-2")
            .expect("no series")
            .values
            .is_empty());
    }

    #[test]
    fn a_run_directory_is_never_reused() {
        let scratch = Scratch::new("reuse");
        assert!(scratch
            .0
            .create_run_dir("run-1")
            .expect("created")
            .is_some());
        assert_eq!(scratch.0.create_run_dir("run-1").expect("taken"), None);
    }

    #[test]
    fn a_damaged_line_is_named_and_does_not_hide_the_others() {
        let scratch = Scratch::new("damaged");
        let good = synced("run-1", "full", 120.0, 1_200);
        fs::create_dir_all(scratch.0.root()).expect("created");
        scratch.0.append_record(&good).expect("appended");
        let path = scratch.0.root().join("runs.jsonl");
        let mut text = fs::read_to_string(&path).expect("read");
        text.push_str("{not json}\n");
        fs::write(&path, text).expect("written");
        scratch.0.append_record(&good).expect("appended");

        let read = scratch.0.records().expect("read");
        assert_eq!(read.values, vec![good.clone(), good]);
        let [damaged] = read.damaged.as_slice() else {
            panic!("one damaged line: {:?}", read.damaged);
        };
        assert!(damaged.contains("line 2"), "{damaged}");
    }

    #[test]
    fn a_sync_with_trouble_around_it_says_so() {
        let mut record = synced("run-1", "full", 120.0, 1_200);
        let synced = synced_of(&mut record);
        assert!(synced.warnings().is_empty());
        synced.restarts = 1;
        synced.build_failures = 3;
        synced.shutdown = Shutdown::Failed {
            cause: "killed".to_owned(),
        };
        synced.disk = DiskUse::Unmeasured {
            cause: "denied".to_owned(),
        };
        let warnings = synced.warnings();
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("restarted itself once")));
        assert!(warnings.iter().any(|w| w.contains("more than once (3)")));
    }

    #[test]
    fn a_sample_before_the_first_block_is_before_the_build() {
        let mut sample = Sample::default();
        assert!(sample.is_before_build());
        sample.target = Some(9);
        assert!(!sample.is_before_build());
    }
}
