use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde_json::value::RawValue;

use super::{MAX_POINTS_PER_SERIES, MAX_SERIES, QueryResult, QueryTiming, ResultType, Series};
use crate::reach::MAX_BODY_BYTES;

#[derive(Deserialize)]
struct Envelope<'a> {
    status: String,
    #[serde(default, borrow)]
    data: Option<Data<'a>>,
    #[serde(default)]
    error: String,
    #[serde(default, rename = "errorType")]
    error_type: String,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    infos: Vec<String>,
}

#[derive(Deserialize)]
struct Data<'a> {
    #[serde(rename = "resultType")]
    result_type: String,
    #[serde(borrow)]
    result: &'a RawValue,
}

#[derive(Deserialize)]
struct WireSeries {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    #[serde(default)]
    value: Option<Sample>,
    #[serde(default)]
    values: Option<Samples>,
    #[serde(default)]
    histogram: Option<IgnoredAny>,
    #[serde(default)]
    histograms: Option<IgnoredAny>,
}

struct Number(f64);

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Numeric;
        impl Visitor<'_> for Numeric {
            type Value = Number;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a numeric sample or its string representation")
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Number, E> {
                Ok(Number(value))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Number, E> {
                Ok(Number(value as f64))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Number, E> {
                Ok(Number(value as f64))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Number, E> {
                value.parse().map(Number).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Numeric)
    }
}

struct Sample {
    time: i64,
    value: Option<f64>,
}

impl<'de> Deserialize<'de> for Sample {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (Number(time), Number(value)) = <(Number, Number)>::deserialize(deserializer)?;
        let millis = (time * 1000.0).round();
        if !millis.is_finite() || millis < i64::MIN as f64 || millis >= i64::MAX as f64 {
            return Err(de::Error::custom(
                "sample timestamp is outside unix milliseconds",
            ));
        }
        Ok(Self {
            time: millis as i64,
            value: value.is_finite().then_some(value),
        })
    }
}

#[derive(Default)]
struct Samples {
    points: Vec<(i64, f64)>,
    gaps: Vec<i64>,
    count: usize,
}

impl Samples {
    fn push<E: de::Error>(&mut self, sample: Sample, previous: &mut Option<i64>) -> Result<(), E> {
        if previous.is_some_and(|time| time >= sample.time) {
            return Err(E::custom(
                "range samples are not in increasing timestamp order",
            ));
        }
        *previous = Some(sample.time);
        match sample.value {
            Some(value) => self.points.push((sample.time, value)),
            None => self.gaps.push(sample.time),
        }
        self.count += 1;
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Samples {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Range;
        impl<'de> Visitor<'de> for Range {
            type Value = Samples;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a range of timestamped numeric samples")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Samples, A::Error> {
                let mut out = Samples::default();
                let mut previous = None;
                while out.count < MAX_POINTS_PER_SERIES {
                    let Some(sample) = seq.next_element::<Sample>()? else {
                        return Ok(out);
                    };
                    out.push(sample, &mut previous)?;
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    out.count += 1;
                }
                Ok(out)
            }
        }
        deserializer.deserialize_seq(Range)
    }
}

struct SeriesList {
    kept: Vec<WireSeries>,
    dropped: usize,
}

impl<'de> Deserialize<'de> for SeriesList {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Matrix;
        impl<'de> Visitor<'de> for Matrix {
            type Value = SeriesList;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array of labelled series")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<SeriesList, A::Error> {
                let mut out = SeriesList {
                    kept: Vec::new(),
                    dropped: 0,
                };
                while out.kept.len() < MAX_SERIES {
                    let Some(series) = seq.next_element()? else {
                        return Ok(out);
                    };
                    out.kept.push(series);
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    out.dropped += 1;
                }
                Ok(out)
            }
        }
        deserializer.deserialize_seq(Matrix)
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<QueryResult, String> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "the Prometheus answer is more than {MAX_BODY_BYTES} bytes; it is hidden"
        ));
    }
    let envelope: Envelope<'_> = serde_json::from_slice(bytes).map_err(json_error)?;
    if envelope.status != "success" {
        return Err(if envelope.error.is_empty() {
            format!("Prometheus status is {}", envelope.status)
        } else if envelope.error_type.is_empty() {
            envelope.error
        } else {
            format!("{}: {}", envelope.error_type, envelope.error)
        });
    }
    let data = envelope.data.ok_or("Prometheus success carried no data")?;
    let result_type = match data.result_type.as_str() {
        "vector" => ResultType::Vector,
        "matrix" => ResultType::Matrix,
        "scalar" => ResultType::Scalar,
        "histogram" => return Err("native histograms are unsupported".to_string()),
        "string" => return Err("Prometheus string result is not a numeric series".to_string()),
        other => {
            return Err(format!(
                "Prometheus resultType {other} is unsupported; expected numeric vector, matrix or scalar"
            ));
        }
    };
    let mut series = Vec::new();
    let mut dropped_series = 0;
    if result_type == ResultType::Scalar {
        let sample: Sample = serde_json::from_str(data.result.get()).map_err(json_error)?;
        series.push(Series {
            labels: Vec::new(),
            points: sample
                .value
                .map(|value| (sample.time, value))
                .into_iter()
                .collect(),
            gaps: sample
                .value
                .is_none()
                .then_some(sample.time)
                .into_iter()
                .collect(),
        });
    } else {
        let list: SeriesList = serde_json::from_str(data.result.get()).map_err(json_error)?;
        dropped_series = list.dropped;
        series.reserve(list.kept.len());
        for wire in list.kept {
            if wire.histogram.is_some() || wire.histograms.is_some() {
                return Err("native histograms in a numeric series are unsupported".to_string());
            }
            let samples = match result_type {
                ResultType::Vector => {
                    let sample = wire.value.ok_or("vector series has no numeric value")?;
                    Samples {
                        points: sample
                            .value
                            .map(|value| (sample.time, value))
                            .into_iter()
                            .collect(),
                        gaps: sample
                            .value
                            .is_none()
                            .then_some(sample.time)
                            .into_iter()
                            .collect(),
                        count: 1,
                    }
                }
                ResultType::Matrix => wire.values.ok_or("matrix series has no numeric samples")?,
                ResultType::Scalar => unreachable!(),
            };
            if samples.count > MAX_POINTS_PER_SERIES {
                dropped_series += 1;
                continue;
            }
            series.push(Series {
                labels: wire.metric.into_iter().collect(),
                points: samples.points,
                gaps: samples.gaps,
            });
        }
    }
    Ok(QueryResult {
        result_type,
        series,
        truncated: dropped_series > 0,
        dropped_series,
        warnings: envelope.warnings,
        infos: envelope.infos,
        source: String::new(),
        timing: QueryTiming::default(),
        step_ms: 0,
        expr: String::new(),
    })
}

fn json_error(error: serde_json::Error) -> String {
    format!("the Prometheus answer is not a supported numeric response: {error}")
}
