//! Trace log queries retain the span's recorded resource and time.
//!
//! A span ID names the origin of the link, while the query selects its trace.
//! Recorded pod identity never inherits the current Kubernetes selection.

use crate::{LokiRequest, QueryWindow, SpanView, quote};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanLogQuery {
    pub request: LokiRequest,
    pub trace_id: String,
    pub span_id: String,
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
}

impl SpanView {
    pub fn log_query(&self, trace_id: &str) -> Result<SpanLogQuery, &'static str> {
        if !valid_id(trace_id, &[16, 32]) {
            return Err("The trace ID must be a nonzero 16 or 32 digit hexadecimal identifier.");
        }
        if !valid_id(&self.id, &[16]) {
            return Err("The span ID must be a nonzero 16 digit hexadecimal identifier.");
        }
        if self.namespace.is_empty() {
            return Err("The span has no Kubernetes namespace.");
        }
        if self.pod.is_empty() {
            return Err("The span has no Kubernetes pod name.");
        }
        if self.start_us == 0 {
            return Err("The span has no start time.");
        }
        let end_us = self
            .start_us
            .checked_add(self.duration_us)
            .and_then(|end| end.checked_add(1_000_000))
            .ok_or("The span end time exceeds the supported log query range.")?;
        let window = QueryWindow {
            start_ms: (self.start_us.saturating_sub(1_000_000) / 1_000) as i64,
            end_ms: end_us.div_ceil(1_000) as i64,
            step_ms: 15_000,
        }
        .validate()?;
        let trace_id = trace_id.to_ascii_lowercase();
        let mut query = format!(
            "{{k8s_namespace_name={},k8s_pod_name={}}}",
            quote(&self.namespace),
            quote(&self.pod),
        );
        if !self.pod_uid.is_empty() {
            query.push_str(" | k8s_pod_uid=");
            query.push_str(&quote(&self.pod_uid));
        }
        query.push_str(" | trace_id=");
        query.push_str(&quote(&trace_id));
        Ok(SpanLogQuery {
            request: LokiRequest {
                query,
                window,
                limit: 200,
            },
            trace_id,
            span_id: self.id.to_ascii_lowercase(),
            namespace: self.namespace.clone(),
            pod: self.pod.clone(),
            pod_uid: self.pod_uid.clone(),
        })
    }
}

fn valid_id(id: &str, widths: &[usize]) -> bool {
    widths.contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && id.bytes().any(|byte| byte != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "5b8efff798038103d269b633813fc700";
    const SPAN: &str = "a9a1b2c3d4e5f678";

    fn span() -> SpanView {
        SpanView {
            id: SPAN.into(),
            parent: String::new(),
            name: "request".into(),
            service: "producer".into(),
            start_us: 1_700_000_000_000_123,
            duration_us: 2_345,
            status: "OK".into(),
            namespace: "prod".into(),
            pod: "producer".into(),
            pod_uid: "recorded-pod".into(),
            node: "node".into(),
        }
    }

    #[test]
    fn the_query_uses_recorded_identity_and_trace_with_a_padded_span_window() {
        let query = span().log_query(TRACE).unwrap();
        assert_eq!(
            query.request.query,
            format!(
                "{{k8s_namespace_name=\"prod\",k8s_pod_name=\"producer\"}} | k8s_pod_uid=\"recorded-pod\" | trace_id=\"{TRACE}\""
            )
        );
        assert_eq!(
            query.request.window,
            QueryWindow {
                start_ms: 1_699_999_999_000,
                end_ms: 1_700_000_001_003,
                step_ms: 15_000,
            }
        );
        assert_eq!(query.request.limit, 200);
        assert_eq!(query.trace_id, TRACE);
        assert_eq!(query.span_id, SPAN);
        assert_eq!(query.namespace, "prod");
        assert_eq!(query.pod, "producer");
        assert_eq!(query.pod_uid, "recorded-pod");
        assert!(!query.request.query.contains("span_id"));
    }

    #[test]
    fn resource_attributes_are_quoted_without_becoming_query_syntax() {
        let mut span = span();
        span.namespace = "prod\"} | trace_id=\"foreign".into();
        span.pod = "pod\\name\n\r\t".into();
        span.pod_uid = "uid\"\\".into();
        let query = span.log_query(TRACE).unwrap();
        assert_eq!(
            query.request.query,
            format!(
                "{{k8s_namespace_name=\"prod\\\"}} | trace_id=\\\"foreign\",k8s_pod_name=\"pod\\\\name\\n\\r\\t\"}} | k8s_pod_uid=\"uid\\\"\\\\\" | trace_id=\"{TRACE}\""
            )
        );
        assert_eq!(query.namespace, span.namespace);
        assert_eq!(query.pod, span.pod);
        assert_eq!(query.pod_uid, span.pod_uid);
    }

    #[test]
    fn a_span_without_uid_keeps_only_its_recorded_name_and_trace_filters() {
        let mut span = span();
        span.pod_uid.clear();
        let query = span.log_query(TRACE).unwrap();
        assert_eq!(
            query.request.query,
            format!(
                "{{k8s_namespace_name=\"prod\",k8s_pod_name=\"producer\"}} | trace_id=\"{TRACE}\""
            )
        );
        assert!(query.pod_uid.is_empty());
    }

    #[test]
    fn foreign_and_recreated_pods_keep_their_own_exact_selectors() {
        let original = span();
        for (namespace, pod, uid) in [
            ("other", "foreign", "foreign-uid"),
            ("prod", "producer", "recreated-uid"),
        ] {
            let span = SpanView {
                namespace: namespace.into(),
                pod: pod.into(),
                pod_uid: uid.into(),
                ..original.clone()
            };
            let query = span.log_query(TRACE).unwrap();
            assert_eq!(
                query.request.query,
                format!(
                    "{{k8s_namespace_name=\"{namespace}\",k8s_pod_name=\"{pod}\"}} | k8s_pod_uid=\"{uid}\" | trace_id=\"{TRACE}\""
                )
            );
            assert_eq!(query.namespace, namespace);
            assert_eq!(query.pod, pod);
            assert_eq!(query.pod_uid, uid);
            assert!(!query.request.query.contains("recorded-pod"));
        }
    }

    #[test]
    fn missing_namespace_pod_and_start_time_have_separate_refusals() {
        let original = span();
        for (span, expected) in [
            (
                SpanView {
                    namespace: String::new(),
                    ..original.clone()
                },
                "The span has no Kubernetes namespace.",
            ),
            (
                SpanView {
                    pod: String::new(),
                    ..original.clone()
                },
                "The span has no Kubernetes pod name.",
            ),
            (
                SpanView {
                    start_us: 0,
                    ..original
                },
                "The span has no start time.",
            ),
        ] {
            assert_eq!(span.log_query(TRACE), Err(expected));
        }
    }

    #[test]
    fn zero_duration_and_submillisecond_spans_round_outward_without_negative_time() {
        for (start_us, duration_us, start_ms, end_ms) in [
            (1, 0, 0, 1_001),
            (999_999, 1, 0, 2_000),
            (1_000_001, 0, 0, 2_001),
            (2_000_001, 1, 1_000, 3_001),
            (2_000_000, 0, 1_000, 3_000),
        ] {
            let query = SpanView {
                start_us,
                duration_us,
                ..span()
            }
            .log_query(TRACE)
            .unwrap();
            assert_eq!(query.request.window.start_ms, start_ms);
            assert_eq!(query.request.window.end_ms, end_ms);
            assert_eq!(query.request.window.validate(), Ok(query.request.window));
        }
    }

    #[test]
    fn duration_and_padding_cannot_overflow_the_span_end() {
        for (start_us, duration_us) in [(u64::MAX, 1), (u64::MAX - 999_999, 0)] {
            assert_eq!(
                SpanView {
                    start_us,
                    duration_us,
                    ..span()
                }
                .log_query(TRACE),
                Err("The span end time exceeds the supported log query range.")
            );
        }
    }

    #[test]
    fn outward_rounding_respects_the_nanosecond_transport_boundary() {
        let maximum_ms = u64::MAX / 1_000_000;
        let span = SpanView {
            start_us: maximum_ms * 1_000 - 1_000_000,
            duration_us: 0,
            ..span()
        };
        let query = span.log_query(TRACE).unwrap();
        assert_eq!(query.request.window.end_ms, maximum_ms as i64);
        assert_eq!(
            SpanView {
                start_us: span.start_us + 1,
                ..span
            }
            .log_query(TRACE),
            Err("the query range or resolution is invalid")
        );
    }

    #[test]
    fn long_spans_do_not_escape_the_existing_day_or_sample_range_limits() {
        for duration_us in [u64::MAX, 86_400_000_000, 75_000_000_000] {
            assert!(
                SpanView {
                    duration_us,
                    ..span()
                }
                .log_query(TRACE)
                .is_err()
            );
        }
        let query = SpanView {
            start_us: 1_000_000_000,
            duration_us: 74_997_999_000,
            ..span()
        }
        .log_query(TRACE)
        .unwrap();
        assert_eq!(
            query.request.window.end_ms - query.request.window.start_ms,
            74_999_999
        );
        assert_eq!(
            SpanView {
                start_us: 1_000_000_000,
                duration_us: 74_998_000_000,
                ..span()
            }
            .log_query(TRACE),
            Err("the query exceeds one day or 5000 samples per series")
        );
    }

    #[test]
    fn both_trace_id_widths_and_uppercase_ids_keep_canonical_lowercase_identity() {
        for trace_id in ["5B8EFFF798038103", "5B8EFFF798038103D269B633813FC700"] {
            let query = SpanView {
                id: SPAN.to_ascii_uppercase(),
                ..span()
            }
            .log_query(trace_id)
            .unwrap();
            assert_eq!(query.trace_id, trace_id.to_ascii_lowercase());
            assert_eq!(query.span_id, SPAN);
            assert!(
                query
                    .request
                    .query
                    .ends_with(&format!(" | trace_id=\"{}\"", query.trace_id))
            );
        }
    }

    #[test]
    fn invalid_or_zero_ids_are_never_used_as_log_filters() {
        for trace_id in [
            "",
            "0000000000000000",
            "00000000000000000000000000000000",
            "abc",
            "5b8efff798038103d269b633813fc70000",
            "5b8efff798038103g269b633813fc700",
            " 5b8efff798038103",
            "5b8efff798038103\n",
        ] {
            assert_eq!(
                span().log_query(trace_id),
                Err("The trace ID must be a nonzero 16 or 32 digit hexadecimal identifier.")
            );
        }
        for id in [
            "",
            "0000000000000000",
            "a9a1b2c3d4e5f67",
            "a9a1b2c3d4e5f6789",
            "a9a1b2c3d4e5f67g",
            "a9a1b2c3d4e5f678a9a1b2c3d4e5f678",
        ] {
            assert_eq!(
                SpanView {
                    id: id.into(),
                    ..span()
                }
                .log_query(TRACE),
                Err("The span ID must be a nonzero 16 digit hexadecimal identifier.")
            );
        }
    }
}
