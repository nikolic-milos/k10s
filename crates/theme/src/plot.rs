//! Fixed-range chart geometry preserves discontinuities before downsampling.

use std::ops::Range;

use crate::Point;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plot {
    pub points: Vec<Point>,
    pub runs: Vec<Range<usize>>,
    pub raw_points: usize,
}

/// Each disconnected run retains its endpoints and bucket extrema. If even
/// the endpoints exceed the budget, the caller gets an explicit refusal.
pub fn prepare_plot(
    points: &[(i64, f64)],
    gaps: &[i64],
    window: Range<i64>,
    step_ms: u64,
    values: Range<f64>,
    max_points: usize,
) -> Result<Plot, &'static str> {
    let mut plot = Plot::default();
    plot.prepare(points, gaps, window, step_ms, values, max_points)?;
    Ok(plot)
}

impl Plot {
    /// Reuse bounded geometry storage across range and resolution changes.
    pub fn prepare(
        &mut self,
        points: &[(i64, f64)],
        gaps: &[i64],
        window: Range<i64>,
        step_ms: u64,
        values: Range<f64>,
        max_points: usize,
    ) -> Result<(), &'static str> {
        self.points.clear();
        self.runs.clear();
        self.raw_points = 0;
        if window.end < window.start
            || step_ms == 0
            || max_points == 0
            || max_points > 16_384
            || values.start > values.end
            || window.end.checked_sub(window.start).is_none()
            || !values.start.is_finite()
            || !values.end.is_finite()
        {
            return Err("invalid chart range, resolution or axis");
        }
        if points
            .iter()
            .any(|(time, value)| *time < window.start || *time > window.end || !value.is_finite())
            || points.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            || gaps.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err("chart samples are outside the window or unordered");
        }
        let mut start = 0;
        let mut endpoints = 0;
        for index in 1..=points.len() {
            let disconnected = index == points.len() || {
                let previous = points[index - 1].0;
                let current = points[index].0;
                let gap = gaps.partition_point(|time| *time <= previous);
                (current - previous) as u64 > step_ms
                    || gaps.get(gap).is_some_and(|time| *time < current)
            };
            if disconnected {
                endpoints += (index - start).min(2);
                if endpoints > max_points {
                    self.runs.clear();
                    return Err("chart has more disconnected runs than its point budget");
                }
                self.runs.push(start..index);
                start = index;
            }
        }
        self.raw_points = points.len();
        let interior = points.len().saturating_sub(endpoints);
        let remaining = max_points.saturating_sub(endpoints);
        let scale = values.start.abs().max(values.end.abs()).max(1.0);
        let min = values.start / scale;
        let span = values.end / scale - min;
        let normalize = |t_ms: i64, value: f64| Point {
            x: ((t_ms - window.start) as f64 / (window.end - window.start).max(1) as f64) as f32,
            y: if span == 0.0 {
                0.5
            } else {
                ((value / scale - min) / span) as f32
            },
        };
        self.points.reserve(points.len().min(max_points));
        for range in &mut self.runs {
            let samples = &points[range.clone()];
            let budget = samples.len().min(2)
                + remaining
                    .checked_mul(samples.len().saturating_sub(2))
                    .ok_or("chart point budget overflow")?
                    .checked_div(interior)
                    .unwrap_or(0);
            let start = self.points.len();
            if samples.len() <= budget {
                self.points
                    .extend(samples.iter().map(|&(time, value)| normalize(time, value)));
            } else {
                self.points.push(normalize(samples[0].0, samples[0].1));
                let bucket_count = budget.saturating_sub(2) / 2;
                if bucket_count > 0 {
                    let interior_count = samples.len() - 2;
                    let mut ordinal = 0;
                    for bucket in 0..bucket_count {
                        let mut min = ordinal + 1;
                        let mut max = min;
                        while ordinal < interior_count
                            && ordinal * bucket_count / interior_count == bucket
                        {
                            let index = ordinal + 1;
                            if samples[index].1 < samples[min].1 {
                                min = index;
                            }
                            if samples[index].1 > samples[max].1 {
                                max = index;
                            }
                            ordinal += 1;
                        }
                        let first = min.min(max);
                        self.points
                            .push(normalize(samples[first].0, samples[first].1));
                        if min != max {
                            let last = min.max(max);
                            self.points
                                .push(normalize(samples[last].0, samples[last].1));
                        }
                    }
                }
                let last = samples[samples.len() - 1];
                self.points.push(normalize(last.0, last.1));
            }
            *range = start..self.points.len();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_geometry_matches_the_extrema_sampler_without_copying_samples() {
        for count in [3, 7, 41, 5000] {
            let points: Vec<_> = (0..count)
                .map(|time| (time as i64, ((time * 73) % 31) as f64))
                .collect();
            let samples: Vec<_> = points
                .iter()
                .map(|&(t_ms, value)| crate::Sample { t_ms, value })
                .collect();
            for budget in [2, 3, 8, 16, 64] {
                let plot = prepare_plot(&points, &[], 0..5000, 1, 0.0..30.0, budget).unwrap();
                let expected: Vec<_> = crate::downsample_samples(&samples, budget)
                    .iter()
                    .map(|sample| Point {
                        x: (sample.t_ms as f64 / 5000.0) as f32,
                        y: (sample.value / 30.0) as f32,
                    })
                    .collect();
                assert_eq!(plot.points, expected, "count={count}, budget={budget}");
            }
        }
    }

    #[test]
    fn range_changes_reuse_storage_and_refusals_clear_previous_geometry() {
        let points: Vec<_> = (0..5000).map(|time| (time, (time % 31) as f64)).collect();
        let mut plot = Plot::default();
        plot.prepare(&points, &[], 0..5000, 1, 0.0..30.0, 64)
            .unwrap();
        let points_storage = plot.points.as_ptr();
        let runs_storage = plot.runs.as_ptr();
        for budget in [16, 64, 32, 8] {
            plot.prepare(&points, &[], 0..6000, 1, 0.0..31.0, budget)
                .unwrap();
            assert_eq!(plot.points.as_ptr(), points_storage);
            assert_eq!(plot.runs.as_ptr(), runs_storage);
            assert!(plot.points.len() <= budget);
        }
        assert!(
            plot.prepare(&points, &[], 0..4998, 1, 0.0..30.0, 64)
                .is_err()
        );
        assert!(plot.points.is_empty());
        assert!(plot.runs.is_empty());
        assert_eq!(plot.raw_points, 0);
        assert!(
            plot.prepare(
                &[(0, 1.0), (20, 1.0), (40, 1.0)],
                &[],
                0..50,
                10,
                1.0..1.0,
                2
            )
            .is_err()
        );
        assert!(plot.points.is_empty());
        assert!(plot.runs.is_empty());
    }

    #[test]
    fn missing_and_non_finite_samples_never_become_a_line() {
        let plot = prepare_plot(
            &[(0, 1.0), (10, 2.0), (30, 4.0), (40, 3.0)],
            &[20],
            0..50,
            10,
            1.0..4.0,
            8,
        )
        .unwrap();
        assert_eq!(plot.runs, vec![0..2, 2..4]);
        assert_eq!(plot.points[2].x, 0.6);
        assert!(
            prepare_plot(
                &[(0, 1.0), (20, 1.0), (40, 1.0)],
                &[],
                0..50,
                10,
                1.0..1.0,
                2
            )
            .is_err()
        );
    }
    #[test]
    fn empty_single_constant_and_extreme_values_have_defined_geometry() {
        assert!(
            prepare_plot(&[], &[], 0..10, 1, 0.0..1.0, 16)
                .unwrap()
                .points
                .is_empty()
        );
        let plot = prepare_plot(&[(5, 2.0)], &[], 0..10, 1, 2.0..2.0, 16).unwrap();
        assert_eq!(plot.points, vec![Point { x: 0.5, y: 0.5 }]);
        let plot = prepare_plot(
            &[(0, -1e308), (10, 1e308)],
            &[],
            0..10,
            10,
            -1e308..1e308,
            16,
        )
        .unwrap();
        assert!(
            plot.points
                .iter()
                .all(|point| point.x.is_finite() && point.y.is_finite())
        );
    }
    #[test]
    fn spikes_survive_and_separate_series_share_the_requested_time_axis() {
        let mut samples: Vec<_> = (0..1000).map(|time| (time, 1.0)).collect();
        samples[451].1 = 10.0;
        let plot = prepare_plot(&samples, &[], 0..2000, 1, 1.0..10.0, 64).unwrap();
        assert!(plot.points.len() <= 64);
        assert!(plot.points.iter().any(|point| point.y == 1.0));
        assert!(plot.points.last().unwrap().x < 0.5);
    }
}
