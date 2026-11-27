//! zainod's `/metrics`: reading a scrape into a [`Sample`], and the analysis
//! of a run's samples: time per stage, and time per band of the chain.

use std::collections::BTreeMap;
use std::fmt;

use crate::record::{IndexStages, Sample, Stages, Timer};

/// Blocks in one band of the chain.
pub(crate) const BAND_BLOCKS: u64 = 10_000;

const WATERMARK: &str = "zaino_store_watermark_height";
const TARGET: &str = "zaino_store_build_target_height";
const INDEX_HEIGHT: &str = "zaino_store_index_tip_height";
const CPU: &str = "process_cpu_seconds_total";
const RESTARTS: &str = "zainod_restarts_total";
const BUILD_FAILURES: &str = "zaino_store_build_failures_total";
const PROCESS_START: &str = "process_start_time_seconds";
const INDEX_LABEL: &str = "index";

/// The pipeline's stages, in pipeline order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    Fetch,
    Resolve,
    QueueWait,
    WriterWait,
    Fold,
    Commit,
}

impl Stage {
    pub(crate) const ALL: [Stage; 6] = [
        Stage::Fetch,
        Stage::Resolve,
        Stage::QueueWait,
        Stage::WriterWait,
        Stage::Fold,
        Stage::Commit,
    ];

    /// The stages zainod times once per index.
    pub(crate) const PER_INDEX: [Stage; 4] = [
        Stage::QueueWait,
        Stage::WriterWait,
        Stage::Fold,
        Stage::Commit,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Stage::Fetch => "fetch",
            Stage::Resolve => "resolve",
            Stage::QueueWait => "queue wait",
            Stage::WriterWait => "writer wait",
            Stage::Fold => "fold",
            Stage::Commit => "commit",
        }
    }

    /// The histogram zainod records the stage in, as a scrape names it.
    fn metric(self) -> &'static str {
        match self {
            Stage::Fetch => "zaino_store_fetch_seconds",
            Stage::Resolve => "zaino_store_resolve_seconds",
            Stage::QueueWait => "zaino_store_queue_wait_seconds",
            Stage::WriterWait => "zaino_store_writer_wait_seconds",
            Stage::Fold => "zaino_store_fold_seconds",
            Stage::Commit => "zaino_store_commit_seconds",
        }
    }
}

impl IndexStages {
    pub(crate) fn timer(&self, stage: Stage) -> Option<Timer> {
        match stage {
            Stage::QueueWait => Some(self.queue_wait),
            Stage::WriterWait => Some(self.writer_wait),
            Stage::Fold => Some(self.fold),
            Stage::Commit => Some(self.commit),
            Stage::Fetch | Stage::Resolve => None,
        }
    }
}

impl Timer {
    /// Mean seconds per block; `None` when no block was timed.
    pub(crate) fn mean_secs(&self) -> Option<f64> {
        (self.blocks > 0).then(|| self.secs / self.blocks as f64)
    }
}

/// Seconds in each stage, the per-index stages summed over the indexes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct StageSecs([f64; 6]);

impl StageSecs {
    pub(crate) fn of(stages: &Stages) -> Self {
        let mut secs = Self::default();
        secs.add(Stage::Fetch, stages.fetch.secs);
        secs.add(Stage::Resolve, stages.resolve.secs);
        for index in stages.indexes.values() {
            for stage in Stage::PER_INDEX {
                if let Some(timer) = index.timer(stage) {
                    secs.add(stage, timer.secs);
                }
            }
        }
        secs
    }

    pub(crate) fn get(&self, stage: Stage) -> f64 {
        Stage::ALL
            .iter()
            .zip(self.0)
            .find_map(|(candidate, secs)| (*candidate == stage).then_some(secs))
            .unwrap_or(0.0)
    }

    fn add(&mut self, stage: Stage, secs: f64) {
        for (candidate, slot) in Stage::ALL.iter().zip(self.0.iter_mut()) {
            if *candidate == stage {
                *slot += secs;
            }
        }
    }

    /// All stages together. Stages run in parallel: this is not wall time.
    pub(crate) fn total(&self) -> f64 {
        self.0.iter().sum()
    }

    /// `later - self`, never negative, times `portion`.
    fn portion_until(&self, later: &Self, portion: f64) -> Self {
        let mut out = Self::default();
        for ((slot, from), to) in out.0.iter_mut().zip(self.0).zip(later.0) {
            *slot = (to - from).max(0.0) * portion;
        }
        out
    }

    fn accumulate(&mut self, other: &Self) {
        for (slot, secs) in self.0.iter_mut().zip(other.0) {
            *slot += secs;
        }
    }
}

/// What a run spent on one band of the chain.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Band {
    /// The band's first height.
    pub(crate) first_height: u64,
    /// Blocks of the band the run built.
    pub(crate) blocks: f64,
    pub(crate) wall_secs: f64,
    pub(crate) stages: StageSecs,
    /// The index that was lowest for most of the band's time.
    pub(crate) slowest: Option<String>,
}

impl Band {
    pub(crate) fn blocks_per_sec(&self) -> Option<f64> {
        (self.wall_secs > 0.0).then(|| self.blocks / self.wall_secs)
    }
}

#[derive(Default)]
struct BandAcc {
    blocks: f64,
    wall_secs: f64,
    stages: StageSecs,
    slowest_secs: BTreeMap<String, f64>,
}

/// Blocks built at a sample: the watermark's height plus genesis.
fn blocks_built(sample: &Sample) -> u64 {
    sample
        .watermark
        .map_or(0, |height| u64::from(height).saturating_add(1))
}

/// The index lowest at a sample; none when several indexes are level, as
/// none of them is then behind the rest.
fn slowest(sample: &Sample) -> Option<&str> {
    let lowest = sample.heights.values().min()?;
    let mut at_lowest = sample
        .heights
        .iter()
        .filter(|(_, height)| *height == lowest)
        .map(|(name, _)| name.as_str());
    let first = at_lowest.next()?;
    match at_lowest.next() {
        Some(_) => None,
        None => Some(first),
    }
}

/// The last sample taken before the store built its first block: where the
/// build's time starts. `None` when the first sample already shows the build.
fn last_before_build(samples: &[Sample]) -> Option<(usize, &Sample)> {
    samples
        .iter()
        .enumerate()
        .take_while(|(_, sample)| sample.is_before_build())
        .last()
}

/// Seconds zainod ran before the store started building (connecting to the
/// validator, building the chain head): no band's time.
pub(crate) fn startup_secs(samples: &[Sample]) -> f64 {
    last_before_build(samples).map_or(0.0, |(_, sample)| sample.elapsed_secs)
}

/// The build's time, band by band, from the last sample before the build
/// (zainod's start when there is none) through the last sample. Time between
/// two samples is shared out over the blocks built between them, evenly;
/// time in which nothing was built goes to the band of the next block, but
/// for time after the store reached its target, which is no band's. Blocks
/// are counted at the lowest index, so the stage time of an index running
/// ahead of it is put in the band the lowest index is in.
pub(crate) fn bands(samples: &[Sample]) -> Vec<Band> {
    let origin = Sample::default();
    let mut acc: BTreeMap<u64, BandAcc> = BTreeMap::new();
    let (skipped, mut previous) = match last_before_build(samples) {
        Some((at, sample)) => (at.saturating_add(1), sample),
        None => (0, &origin),
    };
    for sample in samples.iter().skip(skipped) {
        let wall = (sample.elapsed_secs - previous.elapsed_secs).max(0.0);
        let from = blocks_built(previous);
        let to = blocks_built(sample).max(from);
        let stages_from = StageSecs::of(&previous.stages);
        let stages_to = StageSecs::of(&sample.stages);
        let slowest = slowest(sample);
        let mut share = |band: u64, blocks: f64, portion: f64| {
            let entry = acc.entry(band).or_default();
            entry.blocks += blocks;
            entry.wall_secs += wall * portion;
            entry
                .stages
                .accumulate(&stages_from.portion_until(&stages_to, portion));
            if let Some(name) = slowest {
                *entry.slowest_secs.entry(name.to_owned()).or_default() += wall * portion;
            }
        };
        // built to its target and building nothing: zainod is waiting (for
        // the tool to see it local), which is no band's time
        let waiting = previous.target.is_some() && previous.watermark == previous.target;
        if to == from {
            if !waiting {
                share(from / BAND_BLOCKS, 0.0, 1.0);
            }
        } else {
            let built = to.saturating_sub(from) as f64;
            let mut start = from;
            while start < to {
                let band = start / BAND_BLOCKS;
                let band_end = band.saturating_add(1).saturating_mul(BAND_BLOCKS);
                let end = band_end.min(to);
                let blocks = end.saturating_sub(start) as f64;
                share(band, blocks, blocks / built);
                start = end;
            }
        }
        previous = sample;
    }
    acc.into_iter()
        .map(|(band, acc)| Band {
            first_height: band.saturating_mul(BAND_BLOCKS),
            blocks: acc.blocks,
            wall_secs: acc.wall_secs,
            stages: acc.stages,
            slowest: acc
                .slowest_secs
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(name, _)| name),
        })
        .collect()
}

/// A scrape could not be read.
#[derive(Debug, PartialEq)]
pub(crate) struct MetricsError(String);

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/metrics: {}", self.0)
    }
}

/// One sample line of a scrape: `name{labels} value`.
struct Line<'a> {
    name: &'a str,
    labels: Vec<(String, String)>,
    value: f64,
}

impl Line<'_> {
    fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find_map(|(name, value)| (name == key).then_some(value.as_str()))
    }
}

fn parse_line(line: &str) -> Result<Line<'_>, MetricsError> {
    let malformed = |what: &str| MetricsError(format!("{what} in line `{line}`"));
    let (name, labels, rest) = match line.find(['{', ' ']) {
        Some(at) if line.get(at..).is_some_and(|rest| rest.starts_with('{')) => {
            let name = line.get(..at).ok_or_else(|| malformed("no name"))?;
            let body = line
                .get(at.saturating_add(1)..)
                .ok_or_else(|| malformed("no labels"))?;
            let (labels, rest) = parse_labels(body).ok_or_else(|| malformed("bad labels"))?;
            (name, labels, rest)
        }
        Some(at) => (
            line.get(..at).ok_or_else(|| malformed("no name"))?,
            Vec::new(),
            line.get(at..).ok_or_else(|| malformed("no value"))?,
        ),
        None => return Err(malformed("no value")),
    };
    let value = rest
        .split_whitespace()
        .next()
        .ok_or_else(|| malformed("no value"))?
        .parse::<f64>()
        .map_err(|_| malformed("a value that is not a number"))?;
    Ok(Line {
        name,
        labels,
        value,
    })
}

/// Labels from just after `{` through `}`, and what follows the `}`.
fn parse_labels(body: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels = Vec::new();
    let mut chars = body.char_indices().peekable();
    loop {
        while chars.next_if(|(_, c)| *c == ',' || *c == ' ').is_some() {}
        if let Some((at, _)) = chars.next_if(|(_, c)| *c == '}') {
            return Some((labels, body.get(at.saturating_add(1)..)?));
        }
        let mut key = String::new();
        loop {
            match chars.next()? {
                (_, '=') => break,
                (_, c) => key.push(c),
            }
        }
        if chars.next()?.1 != '"' {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()?.1 {
                '"' => break,
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    c => value.push(c),
                },
                c => value.push(c),
            }
        }
        labels.push((key.trim().to_owned(), value));
    }
}

fn height(line: &Line<'_>) -> Result<u32, MetricsError> {
    let value = line.value;
    if value.is_finite() && value >= 0.0 && value <= f64::from(u32::MAX) && value.fract() == 0.0 {
        // in range and whole: the cast is exact
        Ok(value as u32)
    } else {
        Err(MetricsError(format!(
            "{} is {value}, not a height",
            line.name
        )))
    }
}

/// A counter's value: whole and not negative.
fn counter(line: &Line<'_>) -> Result<u64, MetricsError> {
    let value = line.value;
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 {
        // whole and non-negative: saturates only past u64::MAX
        Ok(value as u64)
    } else {
        Err(MetricsError(format!(
            "{} is {value}, not a count",
            line.name
        )))
    }
}

/// A stage timer's two halves, as they arrive on separate lines.
#[derive(Default)]
struct Halves {
    secs: Option<f64>,
    blocks: Option<f64>,
}

impl Halves {
    fn timer(&self, what: &str) -> Result<Timer, MetricsError> {
        match (self.secs, self.blocks) {
            (Some(secs), Some(blocks))
                if secs.is_finite() && secs >= 0.0 && blocks >= 0.0 && blocks.fract() == 0.0 =>
            {
                Ok(Timer {
                    secs,
                    // whole and non-negative: saturates only past u64::MAX
                    blocks: blocks as u64,
                })
            }
            _ => Err(MetricsError(format!(
                "{what} has no usable sum and count (sum {:?}, count {:?})",
                self.secs, self.blocks
            ))),
        }
    }
}

/// Reads a scrape taken `elapsed_secs` after zainod's start. Heights and
/// timers zainod has not published yet (nothing built so far) are absent or
/// zero; a value that is published but unreadable is an error.
pub(crate) fn parse(text: &str, elapsed_secs: f64) -> Result<Sample, MetricsError> {
    let mut sample = Sample {
        elapsed_secs,
        ..Sample::default()
    };
    let mut timers: BTreeMap<(Stage, Option<String>), Halves> = BTreeMap::new();
    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let name = raw.split(['{', ' ']).next().unwrap_or(raw);
        let timer_half = Stage::ALL.into_iter().find_map(|stage| {
            let suffix = name.strip_prefix(stage.metric())?;
            match suffix {
                "_sum" => Some((stage, true)),
                "_count" => Some((stage, false)),
                _ => None,
            }
        });
        let read = [
            WATERMARK,
            TARGET,
            INDEX_HEIGHT,
            CPU,
            RESTARTS,
            BUILD_FAILURES,
            PROCESS_START,
        ];
        if timer_half.is_none() && !read.contains(&name) {
            continue;
        }
        let line = parse_line(raw)?;
        if let Some((stage, is_sum)) = timer_half {
            let index = line.label(INDEX_LABEL).map(str::to_owned);
            let halves = timers.entry((stage, index)).or_default();
            match is_sum {
                true => halves.secs = Some(line.value),
                false => halves.blocks = Some(line.value),
            }
            continue;
        }
        match line.name {
            WATERMARK => sample.watermark = Some(height(&line)?),
            TARGET => sample.target = Some(height(&line)?),
            INDEX_HEIGHT => {
                let index = line.label(INDEX_LABEL).ok_or_else(|| {
                    MetricsError(format!("{INDEX_HEIGHT} without an index label"))
                })?;
                sample.heights.insert(index.to_owned(), height(&line)?);
            }
            CPU => sample.cpu_secs = Some(line.value),
            RESTARTS => sample.restarts = Some(counter(&line)?),
            BUILD_FAILURES => sample.build_failures = Some(counter(&line)?),
            PROCESS_START => sample.process_start_unix = Some(counter(&line)?),
            _ => {}
        }
    }
    for ((stage, index), halves) in &timers {
        let what = match index {
            Some(index) => format!("{} of {index}", stage.name()),
            None => stage.name().to_owned(),
        };
        let timer = halves.timer(&what)?;
        match (stage, index) {
            (Stage::Fetch, None) => sample.stages.fetch = timer,
            (Stage::Resolve, None) => sample.stages.resolve = timer,
            (Stage::QueueWait | Stage::WriterWait | Stage::Fold | Stage::Commit, Some(index)) => {
                let stages = sample.stages.indexes.entry(index.clone()).or_default();
                match stage {
                    Stage::QueueWait => stages.queue_wait = timer,
                    Stage::WriterWait => stages.writer_wait = timer,
                    Stage::Fold => stages.fold = timer,
                    Stage::Commit => stages.commit = timer,
                    Stage::Fetch | Stage::Resolve => {}
                }
            }
            _ => {
                return Err(MetricsError(format!(
                    "{what}: zainod labels this stage differently than this tool reads it"
                )))
            }
        }
    }
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILDING: &str = include_str!("../fixtures/metrics-building.txt");
    const BEFORE_FIRST_BLOCK: &str = include_str!("../fixtures/metrics-before-first-block.txt");
    const BUILT: &str = include_str!("../fixtures/metrics-built.txt");

    fn sample(elapsed_secs: f64, watermark: Option<u32>, fold_secs: f64) -> Sample {
        let mut sample = Sample {
            elapsed_secs,
            watermark,
            ..Sample::default()
        };
        if let Some(height) = watermark {
            sample.heights.insert("headers".to_owned(), height);
            sample
                .heights
                .insert("txids".to_owned(), height.saturating_add(5));
        }
        sample.stages.indexes.insert(
            "headers".to_owned(),
            IndexStages {
                fold: Timer {
                    secs: fold_secs,
                    blocks: u64::from(watermark.unwrap_or(0)),
                },
                ..IndexStages::default()
            },
        );
        sample
    }

    #[test]
    fn a_scrape_of_a_building_store_is_read() {
        let sample = parse(BUILDING, 12.5).expect("parses");
        assert_eq!(sample.elapsed_secs, 12.5);
        assert!(sample.watermark.is_some(), "{sample:?}");
        assert!(sample.target.is_some(), "{sample:?}");
        assert!(!sample.heights.is_empty());
        assert!(sample.cpu_secs.is_some_and(|secs| secs > 0.0));
        assert_eq!(sample.restarts, Some(0));
        assert_eq!(sample.build_failures, Some(0));
        assert_eq!(sample.process_start_unix, Some(1_795_644_442));
        assert!(!sample.is_before_build());
        assert!(sample.stages.fetch.blocks > 0);
        let watermark = sample.watermark.expect("checked");
        for (index, height) in &sample.heights {
            assert!(*height >= watermark, "{index} is below the watermark");
            let stages = sample.stages.indexes.get(index).expect("timed");
            assert!(stages.fold.blocks > 0, "{index} folded nothing");
            assert!(stages.commit.secs > 0.0, "{index} committed in no time");
        }
    }

    #[test]
    fn a_scrape_before_the_first_block_has_no_heights() {
        let sample = parse(BEFORE_FIRST_BLOCK, 0.5).expect("parses");
        assert_eq!(sample.watermark, None);
        assert_eq!(sample.target, None);
        assert!(sample.heights.is_empty());
        assert_eq!(sample.stages, Stages::default());
        assert!(sample.cpu_secs.is_some());
        assert_eq!(sample.restarts, Some(0));
        assert!(sample.is_before_build());
        // time before the build is startup, and no band's
        let samples = [sample];
        assert_eq!(startup_secs(&samples), 0.5);
        assert!(bands(&samples).is_empty());
    }

    #[test]
    fn a_scrape_of_a_built_store_has_every_index_at_the_target() {
        let sample = parse(BUILT, 9.0).expect("parses");
        let target = sample.target.expect("built");
        assert_eq!(sample.watermark, Some(target));
        assert_eq!(sample.heights.len(), 11, "the full set");
        assert!(sample.heights.values().all(|height| *height == target));
        let blocks = u64::from(target) + 1;
        assert_eq!(sample.stages.fetch.blocks, blocks);
        assert_eq!(sample.stages.resolve.blocks, blocks);
        for (index, stages) in &sample.stages.indexes {
            for stage in Stage::PER_INDEX {
                let timer = stages.timer(stage).expect("a per-index stage");
                assert_eq!(timer.blocks, blocks, "{index} {}", stage.name());
            }
        }
    }

    #[test]
    fn labels_and_values_are_read() {
        let line = parse_line(r#"m_sum{index="a,b",q="x\"y"} 1.5 1700000000"#).expect("parses");
        assert_eq!(line.name, "m_sum");
        assert_eq!(line.label("index"), Some("a,b"));
        assert_eq!(line.label("q"), Some("x\"y"));
        assert_eq!(line.value, 1.5);
        assert_eq!(parse_line("plain 7").expect("parses").value, 7.0);
    }

    #[test]
    fn an_unreadable_value_of_a_metric_read_is_an_error() {
        for scrape in [
            "zaino_store_watermark_height -3",
            "zaino_store_watermark_height 1.5",
            "zaino_store_watermark_height NaN",
            "zaino_store_watermark_height",
            "zaino_store_index_tip_height 7",
            "zaino_store_fetch_seconds_sum 2.0",
            "zaino_store_fold_seconds_sum{index=\"a} 2.0",
            "zainod_restarts_total 1.5",
            "zaino_store_build_failures_total -1",
        ] {
            assert!(parse(scrape, 0.0).is_err(), "{scrape}");
        }
        // a metric this tool does not read is skipped, whatever its shape
        assert!(parse("other{broken 1", 0.0).is_ok());
    }

    #[test]
    fn time_between_samples_is_shared_over_the_bands_crossed() {
        // 0 blocks at 0s (origin); 5,000 at 10s; 25,000 at 50s
        let samples = [
            sample(10.0, Some(4_999), 4.0),
            sample(50.0, Some(24_999), 24.0),
        ];
        let bands = bands(&samples);
        assert_eq!(bands.len(), 3);
        let [first, second, third] = bands.as_slice() else {
            panic!("three bands");
        };
        // band 0: 5,000 blocks in the first 10s, then 5,000 of the next 20,000
        assert_eq!(first.first_height, 0);
        assert_eq!(first.blocks, 10_000.0);
        assert!((first.wall_secs - 20.0).abs() < 1e-9, "{first:?}");
        assert!((first.stages.get(Stage::Fold) - 9.0).abs() < 1e-9);
        // band 1: 10,000 of the 20,000
        assert_eq!(second.first_height, 10_000);
        assert_eq!(second.blocks, 10_000.0);
        assert!((second.wall_secs - 20.0).abs() < 1e-9);
        assert!((second.stages.get(Stage::Fold) - 10.0).abs() < 1e-9);
        assert_eq!(second.blocks_per_sec(), Some(500.0));
        // band 2: the last 5,000
        assert_eq!(third.first_height, 20_000);
        assert_eq!(third.blocks, 5_000.0);
        assert!((third.wall_secs - 10.0).abs() < 1e-9);
        assert_eq!(third.slowest.as_deref(), Some("headers"));
        // nothing is lost: the bands hold all the time and all the stage time
        let wall: f64 = bands.iter().map(|band| band.wall_secs).sum();
        let fold: f64 = bands.iter().map(|band| band.stages.get(Stage::Fold)).sum();
        assert!((wall - 50.0).abs() < 1e-9);
        assert!((fold - 24.0).abs() < 1e-9);
    }

    #[test]
    fn time_before_the_build_is_startup_and_no_bands() {
        // two samples before the first block, then 10,000 blocks by 10s
        let samples = [
            sample(3.0, None, 0.0),
            sample(4.0, None, 0.0),
            sample(10.0, Some(9_999), 1.0),
        ];
        assert_eq!(startup_secs(&samples), 4.0);
        let bands = bands(&samples);
        let [only] = bands.as_slice() else {
            panic!("one band: {bands:?}");
        };
        assert_eq!(only.blocks, 10_000.0);
        assert!((only.wall_secs - 6.0).abs() < 1e-9, "{only:?}");
        assert_eq!(only.blocks_per_sec(), Some(10_000.0 / 6.0));
        // a series that starts in the build has no startup to set aside
        assert_eq!(startup_secs(&samples[2..]), 0.0);
        assert_eq!(startup_secs(&[]), 0.0);
    }

    #[test]
    fn time_in_which_nothing_is_built_goes_to_the_next_blocks_band() {
        let samples = [
            sample(5.0, None, 0.0),
            sample(10.0, Some(9_999), 1.0),
            sample(40.0, Some(9_999), 1.0),
        ];
        let bands = bands(&samples);
        let [first, second] = bands.as_slice() else {
            panic!("two bands: {bands:?}");
        };
        assert_eq!(first.blocks, 10_000.0);
        assert!((first.wall_secs - 5.0).abs() < 1e-9);
        assert_eq!(second.first_height, 10_000);
        assert_eq!(second.blocks, 0.0);
        assert!((second.wall_secs - 30.0).abs() < 1e-9);
        assert_eq!(second.blocks_per_sec(), Some(0.0));
        assert!(super::bands(&[]).is_empty());

        // indexes that are level: none is the slowest
        let mut level = sample(10.0, Some(99), 1.0);
        level.heights.insert("txids".to_owned(), 99);
        assert_eq!(super::bands(&[level])[0].slowest, None);
    }

    /// A chain of exactly two bands, then a wait until the tool sees zainod
    /// local: the wait is no band's, and no band past the chain's end appears.
    #[test]
    fn time_after_the_store_reached_its_target_is_no_bands() {
        let mut built = sample(10.0, Some(19_999), 2.0);
        built.target = Some(19_999);
        let mut seen_local = sample(13.0, Some(19_999), 2.0);
        seen_local.target = Some(19_999);
        let bands = bands(&[built, seen_local]);
        assert_eq!(bands.len(), 2, "{bands:?}");
        let wall: f64 = bands.iter().map(|band| band.wall_secs).sum();
        assert!((wall - 10.0).abs() < 1e-9, "the 3s wait is left out");
        assert!(bands.iter().all(|band| band.blocks == 10_000.0));
    }

    #[test]
    fn the_one_index_of_a_store_is_its_slowest() {
        let mut only = sample(10.0, Some(99), 1.0);
        only.heights.remove("txids");
        assert_eq!(bands(&[only])[0].slowest.as_deref(), Some("headers"));
    }

    #[test]
    fn stage_seconds_sum_the_indexes() {
        let mut stages = Stages {
            fetch: Timer {
                secs: 3.0,
                blocks: 10,
            },
            ..Stages::default()
        };
        for index in ["a", "b"] {
            stages.indexes.insert(
                index.to_owned(),
                IndexStages {
                    fold: Timer {
                        secs: 2.0,
                        blocks: 10,
                    },
                    commit: Timer {
                        secs: 0.5,
                        blocks: 10,
                    },
                    ..IndexStages::default()
                },
            );
        }
        let secs = StageSecs::of(&stages);
        assert_eq!(secs.get(Stage::Fetch), 3.0);
        assert_eq!(secs.get(Stage::Fold), 4.0);
        assert_eq!(secs.get(Stage::Commit), 1.0);
        assert_eq!(secs.total(), 8.0);
        assert_eq!(stages.fetch.mean_secs(), Some(0.3));
        assert_eq!(Timer::default().mean_secs(), None);
    }
}
