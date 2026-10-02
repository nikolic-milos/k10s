use std::sync::Arc;

use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{ListParams, LogParams};
use kube::{Api, Client};

use crate::connect::describe;

// One-shot reads for the inspector panel: recent events for an object and a
// bounded log tail. Fire-and-forget onto the data plane's runtime, the reply
// handed to a caller-supplied callback -- the render thread never blocks on
// the cluster, and a denial arrives as a labelled variant, never an empty
// panel. Both answers are bounded where they enter: an event's fields are
// clipped in characters and a tail line is capped like a followed one.
#[derive(Clone)]
pub struct Inspector {
    client: Client,
    handle: tokio::runtime::Handle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventLine {
    pub last_seen: String,
    pub kind: String,
    pub reason: String,
    pub message: String,
    pub count: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogTail {
    pub lines: Vec<String>,
    /// Why these lines and not the running container's. Present exactly when the
    /// tail is of a container that already exited, because a crash tail that
    /// does not say it is a crash tail reads as the current process being quiet.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InspectDetail {
    Events(Vec<EventLine>),
    Log(LogTail),
    // The probe philosophy carried through: denied is a labelled state the
    // panel renders, not an error string and not silence.
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

const MAX_EVENTS: usize = 20;
const LOG_TAIL_LINES: i64 = 200;
const LOG_LIMIT_BYTES: i64 = 64 * 1024;
const MAX_EVENT_CHARS: usize = 2_000;

impl Inspector {
    pub(crate) fn new(client: Client) -> Inspector {
        Inspector {
            client,
            handle: tokio::runtime::Handle::current(),
        }
    }

    pub fn fetch_events(
        &self,
        namespace: &str,
        name: &str,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) {
        let api: Api<Event> = Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default()
            .fields(&format!("involvedObject.name={name}"))
            .limit(64);
        self.handle.spawn(async move {
            let outcome = match api.list(&params).await {
                Ok(list) => {
                    let mut lines: Vec<EventLine> =
                        list.items.into_iter().map(event_line).collect();
                    lines.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                    lines.truncate(MAX_EVENTS);
                    InspectDetail::Events(lines)
                }
                Err(error) => classify("events", &error),
            };
            reply(outcome);
        });
    }

    /// The tail a person actually wants on a pick. A pod that is crash-looping
    /// has already lost the output that explains why: the running container is
    /// either a fresh process with nothing to say or is not running at all, and
    /// the evidence is in the instance that exited. So the pod is read first,
    /// and a container that has restarted or is waiting to be restarted is
    /// tailed with `previous`, labelled with the reason and exit code the pod
    /// status already carries.
    ///
    /// The fallback matters as much as the choice: a kubelet that has rotated
    /// the previous instance away answers 400, and that is a labelled miss over
    /// the running container rather than an error over nothing.
    pub fn fetch_log_tail(
        &self,
        namespace: &str,
        pod: &Arc<str>,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = pod.clone();
        self.handle.spawn(async move {
            let crashed = match api.get(&pod).await {
                Ok(object) => crashed_container(&object),
                // The pod read is an optimisation, not the request. If it fails
                // the tail is still worth having, unlabelled.
                Err(_) => None,
            };
            let outcome = match tail_with(&api, &pod, crashed.as_ref()).await {
                Ok(detail) => detail,
                Err(error) => classify("logs", &error),
            };
            reply(outcome);
        });
    }
}

/// The container whose *previous* instance holds the explanation, if any.
/// Restarts are the signal, not the phase: a pod can be `Running` with a
/// container that has died forty times.
fn crashed_container(pod: &Pod) -> Option<CrashedContainer> {
    let status = pod.status.as_ref()?;
    let statuses = status.container_statuses.as_deref().unwrap_or_default();
    statuses.iter().find_map(|container| {
        let waiting = container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .and_then(|waiting| waiting.reason.as_deref());
        let last = container
            .last_state
            .as_ref()
            .and_then(|state| state.terminated.as_ref());
        let restarted = container.restart_count > 0;
        if !restarted && last.is_none() {
            return None;
        }
        let mut why = format!("previous instance of {}", container.name);
        if let Some(terminated) = last {
            if let Some(reason) = terminated.reason.as_deref().filter(|r| !r.is_empty()) {
                why.push_str(&format!(", {reason}"));
            }
            why.push_str(&format!(" (exit {})", terminated.exit_code));
        }
        if let Some(waiting) = waiting.filter(|reason| !reason.is_empty()) {
            why.push_str(&format!(", now {waiting}"));
        }
        if container.restart_count > 0 {
            why.push_str(&format!("; {} restarts", container.restart_count));
        }
        Some(CrashedContainer {
            name: container.name.clone(),
            why,
        })
    })
}

struct CrashedContainer {
    name: String,
    why: String,
}

fn log_params(container: Option<&str>, previous: bool) -> LogParams {
    LogParams {
        tail_lines: Some(LOG_TAIL_LINES),
        limit_bytes: Some(LOG_LIMIT_BYTES),
        timestamps: true,
        previous,
        container: container.map(str::to_string),
        ..LogParams::default()
    }
}

async fn tail_with(
    api: &Api<Pod>,
    pod: &str,
    crashed: Option<&CrashedContainer>,
) -> Result<InspectDetail, kube::Error> {
    let Some(crashed) = crashed else {
        let text = api.logs(pod, &log_params(None, false)).await?;
        return Ok(InspectDetail::Log(LogTail {
            lines: tail_lines(&text),
            note: None,
        }));
    };
    match api.logs(pod, &log_params(Some(&crashed.name), true)).await {
        Ok(text) => Ok(InspectDetail::Log(LogTail {
            lines: tail_lines(&text),
            note: Some(crashed.why.clone()),
        })),
        // The kubelet keeps one previous instance and drops it on rotation or a
        // node restart, so "not found" here is a fact about retention.
        Err(_) => {
            let text = api
                .logs(pod, &log_params(Some(&crashed.name), false))
                .await?;
            Ok(InspectDetail::Log(LogTail {
                lines: tail_lines(&text),
                note: Some(format!(
                    "{}; previous log is gone from the node",
                    crashed.why
                )),
            }))
        }
    }
}

fn event_line(event: Event) -> EventLine {
    EventLine {
        last_seen: clipped(
            event
                .last_timestamp
                .map(|t| t.0.to_string())
                .or_else(|| event.event_time.map(|t| t.0.to_string()))
                .unwrap_or_default(),
        ),
        kind: clipped(event.type_.unwrap_or_default()),
        reason: clipped(event.reason.unwrap_or_default()),
        message: clipped(event.message.unwrap_or_default()),
        count: event.count.unwrap_or(1),
    }
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_EVENT_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_EVENT_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn tail_lines(text: &str) -> Vec<String> {
    text.lines().map(crate::logs::capped).collect()
}

fn classify(what: &'static str, error: &kube::Error) -> InspectDetail {
    if let kube::Error::Api(response) = error
        && matches!(response.code, 401 | 403)
    {
        return InspectDetail::Denied { what };
    }
    InspectDetail::Failed {
        what,
        why: describe(error as &(dyn std::error::Error + 'static)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_bounded_where_it_enters_rather_than_by_the_panel() {
        let line = event_line(Event {
            message: Some("m".repeat(MAX_EVENT_CHARS * 4)),
            reason: Some("é".repeat(MAX_EVENT_CHARS + 10)),
            type_: Some("Warning".to_string()),
            count: Some(7),
            ..Event::default()
        });
        assert_eq!(
            line.message.chars().count(),
            MAX_EVENT_CHARS + 1,
            "the cut is a character and the ellipsis says it happened"
        );
        assert!(line.message.ends_with('\u{2026}'));
        assert_eq!(
            line.reason.chars().count(),
            MAX_EVENT_CHARS + 1,
            "a multibyte field is cut in characters, not bytes"
        );
        assert!(line.reason.ends_with('\u{2026}'));
        assert_eq!(
            line.kind, "Warning",
            "a field inside the budget is untouched"
        );
        assert_eq!(line.count, 7);
        assert_eq!(
            event_line(Event::default()).count,
            1,
            "an event that counted nothing happened once"
        );
    }

    #[test]
    fn an_account_the_cluster_refuses_is_denied_whichever_way_it_says_so() {
        let refused = |code: u16| {
            kube::Error::Api(
                kube::core::Status::failure("no", "Forbidden")
                    .with_code(code)
                    .boxed(),
            )
        };
        assert_eq!(
            classify("events", &refused(403)),
            InspectDetail::Denied { what: "events" }
        );
        assert_eq!(
            classify("events", &refused(401)),
            InspectDetail::Denied { what: "events" },
            "the panel reads 401 the way the watch and the reader do"
        );
        assert!(matches!(
            classify("logs", &refused(500)),
            InspectDetail::Failed { .. }
        ));
    }

    #[test]
    fn a_log_tail_line_is_capped_the_way_a_followed_line_is() {
        let text = format!("{}\nshort\n", "x".repeat(64 * 1024));
        let lines = tail_lines(&text);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].len() < 64 * 1024,
            "one mega-line does not reach the panel whole"
        );
        assert!(lines[0].ends_with('\u{2026}'));
        assert_eq!(lines[1], "short");
    }
}
