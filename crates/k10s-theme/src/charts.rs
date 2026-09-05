//! Chart primitives in theme colours: sparklines and a bounded series strip.
//!
//! Geometry only. The map painter turns these into quads; this module never
//! talks to Grafana, Prometheus, or gpui's scene. A sparkline is a polyline
//! in unit space so a card can stamp it at any size without resampling.

/// One sample. Time is unix milliseconds; value is the already-parsed number
/// Prometheus/Loki returned. NaN and Inf are dropped at construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t_ms: i64,
    pub value: f64,
}

/// A named series the overlay can stamp onto a card or a panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Map samples onto a unit square (0,0) bottom-left to (1,1) top-right.
/// Empty or single-point series yield no segments: a sparkline of one dot is
/// a mark, not a line, and the painter already has marks.
pub fn sparkline(samples: &[Sample]) -> Vec<Point> {
    sparkline_bounded(samples, usize::MAX)
}

/// Keep at most `max_points` samples, retaining extrema in every time bucket.
/// Overlay caches call this off the paint path so a card never walks a matrix.
pub fn downsample_samples(samples: &[Sample], max_points: usize) -> Vec<Sample> {
    if max_points < 2 {
        return Vec::new();
    }

    let mut finite = Vec::new();
    let mut first = None;
    let mut last = None;
    for (index, sample) in samples.iter().copied().enumerate() {
        if !sample.value.is_finite() {
            continue;
        }
        first.get_or_insert((index, sample));
        last = Some((index, sample));
        finite.push((index, sample));
    }
    if finite.len() < 2 {
        return Vec::new();
    }
    if finite.len() <= max_points {
        return finite.into_iter().map(|(_, sample)| sample).collect();
    }

    let (first_index, first_sample) = first.expect("two finite samples have a first");
    let (last_index, last_sample) = last.expect("two finite samples have a last");
    if max_points == 2 {
        return vec![first_sample, last_sample];
    }

    #[derive(Clone, Copy, Default)]
    struct Extrema {
        min: Option<(usize, Sample)>,
        max: Option<(usize, Sample)>,
    }

    let bucket_count = (max_points - 2) / 2;
    if bucket_count == 0 {
        return vec![first_sample, last_sample];
    }
    let interior_count = finite.len() - 2;
    let mut buckets = vec![Extrema::default(); bucket_count];
    let mut ordinal = 0usize;
    for (index, sample) in finite {
        if index == first_index || index == last_index {
            continue;
        }
        let bucket = (ordinal * bucket_count / interior_count).min(bucket_count - 1);
        ordinal += 1;
        let extrema = &mut buckets[bucket];
        if extrema
            .min
            .is_none_or(|(_, current)| sample.value < current.value)
        {
            extrema.min = Some((index, sample));
        }
        if extrema
            .max
            .is_none_or(|(_, current)| sample.value > current.value)
        {
            extrema.max = Some((index, sample));
        }
    }

    let mut kept = Vec::with_capacity(max_points);
    kept.push(first_sample);
    for extrema in buckets {
        match (extrema.min, extrema.max) {
            (Some(min), Some(max)) if min.0 < max.0 => {
                kept.push(min.1);
                kept.push(max.1);
            }
            (Some(min), Some(max)) if min.0 > max.0 => {
                kept.push(max.1);
                kept.push(min.1);
            }
            (Some(point), _) | (_, Some(point)) => kept.push(point.1),
            (None, None) => {}
        }
    }
    kept.push(last_sample);
    kept
}

/// Normalize at most `max_points` samples while retaining extrema in every
/// time bucket. The fixed map budget can therefore discard resolution without
/// flattening a short CPU or error spike into an average.
pub fn sparkline_bounded(samples: &[Sample], max_points: usize) -> Vec<Point> {
    let kept = downsample_samples(samples, max_points);
    if kept.len() < 2 {
        return Vec::new();
    }
    let t0 = kept.iter().map(|sample| sample.t_ms).min().unwrap_or(0);
    let t1 = kept.iter().map(|sample| sample.t_ms).max().unwrap_or(0);
    let min_v = kept
        .iter()
        .map(|sample| sample.value)
        .fold(f64::INFINITY, f64::min);
    let max_v = kept
        .iter()
        .map(|sample| sample.value)
        .fold(f64::NEG_INFINITY, f64::max);
    kept.into_iter()
        .map(|sample| Point {
            x: (sample.t_ms - t0) as f32 / (t1 - t0).max(1) as f32,
            y: (sample.value - min_v) as f32 / (max_v - min_v).max(f64::EPSILON) as f32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_points_span_the_unit_square() {
        let pts = sparkline(&[
            Sample {
                t_ms: 1_000,
                value: 10.0,
            },
            Sample {
                t_ms: 2_000,
                value: 20.0,
            },
        ]);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0], Point { x: 0.0, y: 0.0 });
        assert_eq!(pts[1], Point { x: 1.0, y: 1.0 });
    }

    #[test]
    fn nan_and_inf_are_dropped_and_one_point_is_not_a_line() {
        assert!(
            sparkline(&[Sample {
                t_ms: 1,
                value: f64::NAN
            }])
            .is_empty()
        );
        assert!(
            sparkline(&[Sample {
                t_ms: 1,
                value: 1.0
            }])
            .is_empty()
        );
        let pts = sparkline(&[
            Sample {
                t_ms: 1,
                value: f64::INFINITY,
            },
            Sample {
                t_ms: 2,
                value: 1.0,
            },
            Sample {
                t_ms: 3,
                value: 3.0,
            },
        ]);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].y, 0.0);
        assert_eq!(pts[1].y, 1.0);
    }

    #[test]
    fn bounded_geometry_keeps_a_short_spike_in_a_large_series() {
        let mut samples: Vec<_> = (0..10_000)
            .map(|t_ms| Sample { t_ms, value: 1.0 })
            .collect();
        samples[4_321].value = 1_000.0;

        let points = sparkline_bounded(&samples, 64);

        assert!(points.len() <= 64);
        assert_eq!(points.first().map(|point| point.x), Some(0.0));
        assert_eq!(points.last().map(|point| point.x), Some(1.0));
        assert!(points.iter().any(|point| point.y == 1.0));
    }

    #[test]
    fn bounded_geometry_is_ordered_even_when_extrema_reverse_in_a_bucket() {
        let samples: Vec<_> = (0..100)
            .map(|t_ms| Sample {
                t_ms,
                value: if t_ms % 10 == 1 { 10.0 } else { t_ms as f64 },
            })
            .collect();

        let points = sparkline_bounded(&samples, 12);

        assert!(points.windows(2).all(|pair| pair[0].x <= pair[1].x));
    }

    #[test]
    fn downsample_keeps_extrema_as_samples_not_only_geometry() {
        let mut samples: Vec<_> = (0..1_000).map(|t_ms| Sample { t_ms, value: 1.0 }).collect();
        samples[100].value = 40.0;
        let kept = downsample_samples(&samples, 16);
        assert!(kept.len() <= 16);
        assert!(kept.iter().any(|sample| sample.value == 40.0));
        assert_eq!(kept.first().map(|sample| sample.t_ms), Some(0));
        assert_eq!(kept.last().map(|sample| sample.t_ms), Some(999));
    }
}
