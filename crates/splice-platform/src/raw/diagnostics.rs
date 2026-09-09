use std::collections::BTreeMap;

struct Distribution {
    count: u64,
    sum: u128,
    max: u64,
    buckets: [u64; 64],
}

impl Default for Distribution {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0,
            max: 0,
            buckets: [0; 64],
        }
    }
}

#[derive(serde::Serialize)]
struct Summary {
    count: u64,
    mean_us: u64,
    max_us: u64,
    p50_upper_us: u64,
    p95_upper_us: u64,
    p99_upper_us: u64,
}

impl Distribution {
    fn record(&mut self, value: u64) {
        self.count += 1;
        self.sum += u128::from(value);
        self.max = self.max.max(value);
        let bucket = (64 - value.leading_zeros()).min(63) as usize;
        self.buckets[bucket] += 1;
    }

    fn percentile(&self, percent: u64) -> u64 {
        let rank = (self.count * percent).div_ceil(100);
        let mut count = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            count += bucket;
            if count >= rank {
                return if index == 63 {
                    self.max
                } else {
                    ((1u64 << index) - 1).min(self.max)
                };
            }
        }
        self.max
    }

    fn summary(&self) -> Summary {
        Summary {
            count: self.count,
            mean_us: (self.sum / u128::from(self.count)) as u64,
            max_us: self.max,
            p50_upper_us: self.percentile(50),
            p95_upper_us: self.percentile(95),
            p99_upper_us: self.percentile(99),
        }
    }
}

pub struct Diagnostics {
    scope: &'static str,
    started_us: u64,
    metrics: BTreeMap<&'static str, Distribution>,
}

impl Diagnostics {
    pub fn new(scope: &'static str) -> Self {
        Self {
            scope,
            started_us: super::clock::now_us(),
            metrics: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, metric: &'static str, microseconds: u64) {
        self.metrics.entry(metric).or_default().record(microseconds);
    }

    pub fn flush_if_due(&mut self, now_us: u64) {
        if now_us.saturating_sub(self.started_us) >= 10_000_000 {
            self.flush(now_us);
        }
    }

    pub fn finish_window(&mut self) {
        self.flush(super::clock::now_us());
    }

    fn flush(&mut self, now_us: u64) {
        if !self.metrics.is_empty() {
            let summary: BTreeMap<_, _> = self
                .metrics
                .iter()
                .map(|(name, values)| (*name, values.summary()))
                .collect();
            tracing::info!(scope = self.scope, elapsed_us = now_us.saturating_sub(self.started_us), measurements = %serde_json::to_string(&summary).expect("timing summaries serialize"), "raw input timing");
            self.metrics.clear();
        }
        self.started_us = now_us;
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        self.flush(super::clock::now_us());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_reports_exact_counts_mean_max_and_bounded_percentiles() {
        let mut values = Distribution::default();
        for value in 0..1000 {
            values.record(value);
        }
        let summary = values.summary();
        assert_eq!(
            (summary.count, summary.mean_us, summary.max_us),
            (1000, 499, 999)
        );
        assert!((499..=998).contains(&summary.p50_upper_us));
        assert!((949..=999).contains(&summary.p95_upper_us));
        assert!((989..=999).contains(&summary.p99_upper_us));
    }
}
