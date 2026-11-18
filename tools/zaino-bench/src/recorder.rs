//! An in-process `metrics` recorder that aggregates: counters and gauges as
//! numbers, histograms as count, sum, max and a log-spaced bucket table for
//! percentiles. Holds no samples, so a multi-million-block build costs it
//! nothing.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// Log-spaced buckets: `BUCKET_BASE_NS * 2^i` nanoseconds, i in 0..BUCKETS.
const BUCKETS: usize = 40;
const BUCKET_BASE_NS: f64 = 100.0;

struct Hist {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

impl Default for Hist {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Hist {
    fn record(&self, seconds: f64) {
        let ns = (seconds * 1e9).max(0.0) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        let bucket =
            ((ns as f64 / BUCKET_BASE_NS).log2().floor().max(0.0) as usize).min(BUCKETS - 1);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn quantile(&self, q: f64) -> f64 {
        let total = self.count.load(Ordering::Relaxed);
        let target = (total as f64 * q).ceil() as u64;
        let mut seen = 0;
        for (i, bucket) in self.buckets.iter().enumerate() {
            seen += bucket.load(Ordering::Relaxed);
            if seen >= target {
                return BUCKET_BASE_NS * 2f64.powi(i as i32 + 1) / 1e9;
            }
        }
        self.max_ns.load(Ordering::Relaxed) as f64 / 1e9
    }
}

#[derive(Default)]
struct Store {
    counters: BTreeMap<String, Arc<AtomicU64>>,
    gauges: BTreeMap<String, Arc<AtomicU64>>,
    histograms: BTreeMap<String, Arc<Hist>>,
}

/// The recorder; `Snapshotter` reads it.
#[derive(Clone, Default)]
pub struct Aggregating {
    store: Arc<Mutex<Store>>,
}

/// Reads the recorder's state.
pub struct Snapshotter(Aggregating);

fn label(key: &Key) -> String {
    let labels: Vec<String> = key
        .labels()
        .map(|l| format!("{}={}", l.key(), l.value()))
        .collect();
    match labels.is_empty() {
        true => key.name().to_owned(),
        false => format!("{}{{{}}}", key.name(), labels.join(",")),
    }
}

struct CounterHandle(Arc<AtomicU64>);
impl metrics::CounterFn for CounterHandle {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    fn absolute(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

struct GaugeHandle(Arc<AtomicU64>);
impl metrics::GaugeFn for GaugeHandle {
    fn increment(&self, value: f64) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
    }
    fn decrement(&self, value: f64) {
        self.increment(-value);
    }
    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

struct HistHandle(Arc<Hist>);
impl metrics::HistogramFn for HistHandle {
    fn record(&self, value: f64) {
        self.0.record(value);
    }
}

impl Recorder for Aggregating {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let cell = self.cell(key, |store| &mut store.counters);
        Counter::from_arc(Arc::new(CounterHandle(cell)))
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let cell = self.cell(key, |store| &mut store.gauges);
        Gauge::from_arc(Arc::new(GaugeHandle(cell)))
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let hist = self.cell(key, |store| &mut store.histograms);
        Histogram::from_arc(Arc::new(HistHandle(hist)))
    }
}

impl Aggregating {
    /// The cell `key` names in the map `pick` selects, created on first use.
    fn cell<T: Default>(
        &self,
        key: &Key,
        pick: impl FnOnce(&mut Store) -> &mut BTreeMap<String, Arc<T>>,
    ) -> Arc<T> {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(pick(&mut store).entry(label(key)).or_default())
    }
}

/// Installs the recorder for the process; the returned snapshotter reads it.
pub fn install() -> Result<Snapshotter, String> {
    let recorder = Aggregating::default();
    metrics::set_global_recorder(recorder.clone())
        .map_err(|error| format!("metrics recorder: {error}"))?;
    Ok(Snapshotter(recorder))
}

/// One histogram, aggregated.
#[derive(Clone, Debug, serde::Serialize)]
pub struct HistogramSummary {
    pub count: u64,
    pub total_seconds: f64,
    pub mean_seconds: f64,
    pub p50_seconds: f64,
    pub p99_seconds: f64,
    pub max_seconds: f64,
}

/// Every metric the process recorded.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Snapshot {
    pub counters: BTreeMap<String, u64>,
    pub gauges: BTreeMap<String, f64>,
    pub histograms: BTreeMap<String, HistogramSummary>,
}

impl Snapshotter {
    pub fn snapshot(&self) -> Snapshot {
        let store = self.0.store.lock().unwrap_or_else(PoisonError::into_inner);
        Snapshot {
            counters: store
                .counters
                .iter()
                .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
                .collect(),
            gauges: store
                .gauges
                .iter()
                .map(|(k, v)| (k.clone(), f64::from_bits(v.load(Ordering::Relaxed))))
                .collect(),
            histograms: store
                .histograms
                .iter()
                .map(|(k, h)| {
                    let count = h.count.load(Ordering::Relaxed);
                    let total = h.sum_ns.load(Ordering::Relaxed) as f64 / 1e9;
                    (
                        k.clone(),
                        HistogramSummary {
                            count,
                            total_seconds: total,
                            mean_seconds: if count == 0 {
                                0.0
                            } else {
                                total / count as f64
                            },
                            p50_seconds: h.quantile(0.5),
                            p99_seconds: h.quantile(0.99),
                            max_seconds: h.max_ns.load(Ordering::Relaxed) as f64 / 1e9,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "metrics:")?;
        for (name, value) in &self.counters {
            writeln!(f, "  {name} = {value}")?;
        }
        for (name, value) in &self.gauges {
            writeln!(f, "  {name} = {value}")?;
        }
        for (name, h) in &self.histograms {
            writeln!(
                f,
                "  {name}: n={} total={:.1}s mean={:.3}ms p50={:.3}ms p99={:.3}ms max={:.3}ms",
                h.count,
                h.total_seconds,
                h.mean_seconds * 1e3,
                h.p50_seconds * 1e3,
                h.p99_seconds * 1e3,
                h.max_seconds * 1e3,
            )?;
        }
        Ok(())
    }
}
