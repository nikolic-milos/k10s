//! The benchmark harness and the baseline comparator.
//!
//! Timing batches short operations until one independent sample spans at
//! least 20 us and reports sample count, batch size, and median relative
//! absolute deviation alongside every number. The comparator in `baseline` is
//! fail-closed: a missing suite, a schema drift, a vanished case, a structural
//! counter change, a noisy median, or a sample-count collapse is a rejection,
//! not a warning -- a gate that can be disabled by the regression it guards
//! against is not a gate.

use std::time::{Duration, Instant};

pub mod baseline;
pub mod dependency;

pub const P99_MIN_SAMPLES: usize = 100;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub warmup_calls: usize,
    pub min_samples: usize,
    pub max_samples: usize,
    pub budget: Duration,
    pub target_sample_time: Duration,
}

impl Config {
    pub const fn new(
        warmup_calls: usize,
        min_samples: usize,
        max_samples: usize,
        budget: Duration,
    ) -> Self {
        Config {
            warmup_calls,
            min_samples,
            max_samples,
            budget,
            target_sample_time: Duration::from_micros(20),
        }
    }
}

#[derive(Debug)]
pub struct Samples {
    per_call_ns: Vec<f64>,
    batch_size: usize,
}

impl Samples {
    pub fn sample_count(&self) -> usize {
        self.per_call_ns.len()
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn iterations(&self) -> usize {
        self.sample_count() * self.batch_size
    }

    pub fn percentile(&self, percentile: f64) -> f64 {
        assert!((0.0..=1.0).contains(&percentile));
        let index = (((self.per_call_ns.len() - 1) as f64) * percentile).round() as usize;
        self.per_call_ns[index]
    }

    pub fn p50_relative_mad(&self) -> f64 {
        let median = self.percentile(0.5);
        if median == 0.0 {
            return 0.0;
        }
        let mut deviations: Vec<f64> = self
            .per_call_ns
            .iter()
            .map(|sample| (sample - median).abs())
            .collect();
        deviations.sort_unstable_by(f64::total_cmp);
        deviations[(deviations.len() - 1) / 2] / median
    }

    pub fn tail_label(&self) -> &'static str {
        if self.sample_count() >= P99_MIN_SAMPLES {
            "p99"
        } else {
            "max"
        }
    }

    pub fn tail(&self) -> f64 {
        if self.sample_count() >= P99_MIN_SAMPLES {
            self.percentile(0.99)
        } else {
            *self.per_call_ns.last().unwrap_or(&0.0)
        }
    }
}

pub fn measure(config: Config, mut run: impl FnMut()) -> Samples {
    assert!(config.min_samples > 0);
    assert!(config.min_samples <= config.max_samples);

    for _ in 0..config.warmup_calls {
        run();
    }

    let batch_size = calibrate_batch(config.target_sample_time, &mut run);
    let mut per_call_ns = Vec::with_capacity(config.min_samples);
    let start = Instant::now();
    while per_call_ns.len() < config.max_samples
        && (per_call_ns.len() < config.min_samples || start.elapsed() < config.budget)
    {
        let sample_start = Instant::now();
        for _ in 0..batch_size {
            run();
        }
        per_call_ns.push(sample_start.elapsed().as_nanos() as f64 / batch_size as f64);
    }
    per_call_ns.sort_unstable_by(f64::total_cmp);
    Samples {
        per_call_ns,
        batch_size,
    }
}

fn calibrate_batch(target: Duration, run: &mut impl FnMut()) -> usize {
    const MAX_BATCH_SIZE: usize = 1 << 20;

    let target_ns = target.as_nanos().max(1);
    let mut batch_size = 1usize;
    loop {
        let start = Instant::now();
        for _ in 0..batch_size {
            run();
        }
        let elapsed_ns = start.elapsed().as_nanos().max(1);
        if elapsed_ns >= target_ns || batch_size == MAX_BATCH_SIZE {
            return batch_size;
        }
        let scale = target_ns.div_ceil(elapsed_ns).clamp(2, 64) as usize;
        batch_size = batch_size.saturating_mul(scale).min(MAX_BATCH_SIZE);
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use super::*;

    #[test]
    fn short_operations_are_batched_and_counted() {
        let samples = measure(
            Config {
                target_sample_time: Duration::from_micros(10),
                ..Config::new(4, 12, 12, Duration::ZERO)
            },
            || black_box(()),
        );
        assert_eq!(samples.sample_count(), 12);
        assert!(samples.batch_size() > 1);
        assert_eq!(
            samples.iterations(),
            samples.sample_count() * samples.batch_size()
        );
        assert!(samples.percentile(0.5).is_finite());
        assert_eq!(samples.tail_label(), "max");
    }
}
