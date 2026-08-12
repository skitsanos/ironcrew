use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(crate) const DURATION_BUCKETS_MICROS: [u64; 15] = [
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    30_000_000,
    60_000_000,
    120_000_000,
    300_000_000,
];

pub(crate) const DURATION_BUCKET_LABELS: [&str; DURATION_BUCKETS_MICROS.len()] = [
    "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "30", "60",
    "120", "300",
];

const EXCLUSIVE_BUCKET_COUNT: usize = DURATION_BUCKETS_MICROS.len() + 1;

pub(crate) struct Histogram {
    /// Mutually exclusive finite buckets plus one overflow bucket. Prometheus
    /// cumulative buckets are derived from one loaded snapshot, so concurrent
    /// scrapes cannot observe a lower bound exceeding a later bound.
    buckets: [AtomicU64; EXCLUSIVE_BUCKET_COUNT],
    sum_micros: AtomicU64,
}

pub(crate) struct HistogramSnapshot {
    pub(crate) buckets: [u64; EXCLUSIVE_BUCKET_COUNT],
    pub(crate) sum_micros: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_micros: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub(crate) fn record(&self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        saturating_add(&self.sum_micros, micros);
        let bucket = DURATION_BUCKETS_MICROS
            .iter()
            .position(|upper_bound| micros <= *upper_bound)
            .unwrap_or(DURATION_BUCKETS_MICROS.len());
        saturating_add(&self.buckets[bucket], 1);
    }

    pub(crate) fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed)),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

pub(crate) fn saturating_add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}
