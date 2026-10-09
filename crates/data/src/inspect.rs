//! Inspector events belong to one object UID, not every use of its name.
//!
//! Crash tails name the previous container. Only the kubelet's explicit
//! missing-previous response or an exact runtime refusal naming the recorded
//! last terminated container ID permits a labelled current-container fallback;
//! metadata, authorization and backend failures retain their own outcomes.
//! Nonempty tails must begin with the requested kubelet timestamp. In-band
//! refusals keep the server's sentence instead of becoming application lines.
//! Tails request 200 lines and 64 KiB from the kubelet. Successful bodies are
//! also locally capped at 64 KiB; the SDK still buffers HTTP error bodies.
//! A selected Pod UID is checked before and after a successful tail. Logs are
//! addressed by name, so these checks cannot attest the kubelet response UID.
//! Events and tails share the Reader's scheduler and interactive client. Their
//! guard cancels queued or active work; one 35-second deadline covers both.

use std::future::Future;
use std::sync::Arc;

use futures::{AsyncRead, AsyncReadExt};
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{GetParams, ListParams, LogParams};
use kube::core::Request;
use kube::{Api, Client};
use serde::Deserialize;

use crate::read::{Fetched, server_sentence};
use crate::telemetry::{RequestStop, Scheduler};

// One-shot reads for the inspector panel: recent events for an object and a
// bounded log tail. The caller owns each request until its reply or cancellation.
#[derive(Clone)]
pub struct Inspector {
    client: Client,
    handle: tokio::runtime::Handle,
    scheduler: Arc<Scheduler>,
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
    /// A crash tail names the previous container; a fallback names the current
    /// container and the server's refusal of the previous request.
    pub note: Option<String>,
    /// Only an explicit missing-previous response or an exact runtime refusal
    /// naming the recorded last terminated container permits retained history.
    pub previous_unavailable: bool,
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
const RUNTIME_LOG_REFUSAL: &str = "unable to retrieve container logs for ";

impl Inspector {
    pub(crate) fn new(client: Client, scheduler: Arc<Scheduler>) -> Inspector {
        Inspector {
            client,
            handle: tokio::runtime::Handle::current(),
            scheduler,
        }
    }

    pub fn fetch_events(
        &self,
        namespace: &str,
        uid: &str,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) -> RequestStop {
        if uid.is_empty() {
            reply(InspectDetail::Failed {
                what: "events",
                why: "the selected object has no UID; its events cannot be scoped".to_string(),
            });
            return RequestStop::noop();
        }
        let api: Api<Event> = Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default()
            .fields(&format!("involvedObject.uid={uid}"))
            .limit(64);
        self.submit(
            async move {
                match api.list(&params).await {
                    Ok(list) => {
                        let mut lines: Vec<EventLine> =
                            list.items.into_iter().map(event_line).collect();
                        lines.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                        lines.truncate(MAX_EVENTS);
                        InspectDetail::Events(lines)
                    }
                    Err(error) => classify("events", &error),
                }
            },
            reply,
        )
    }

    /// The tail a person actually wants on a pick. A pod that is crash-looping
    /// has already lost the output that explains why: the running container is
    /// either a fresh process with nothing to say or is not running at all, and
    /// the evidence is in the instance that exited. So the pod is read first,
    /// and a container that has restarted or is waiting to be restarted is
    /// tailed with `previous`, labelled with the reason and exit code the pod
    /// status already carries.
    ///
    /// The kubelet's missing-previous response or an exact runtime refusal for
    /// the recorded terminated container permits current-container logs with
    /// the original refusal stated. Other errors never imply lost history.
    /// The status read and a metadata-only read after the tail reject name
    /// reuse before publishing any lines, without a backend UID guarantee.
    pub fn fetch_log_tail(
        &self,
        namespace: &str,
        pod: &Arc<str>,
        uid: &str,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) -> RequestStop {
        if uid.is_empty() {
            reply(InspectDetail::Failed {
                what: "logs",
                why: "the selected Pod has no UID; its log tail cannot be scoped".to_string(),
            });
            return RequestStop::noop();
        }
        let client = self.client.clone();
        let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let pod = pod.clone();
        let uid = uid.to_string();
        self.submit(
            async move {
                let crashed = match api.get(&pod).await {
                    Ok(object) => {
                        if let Err(detail) = check_uid(&pod, &uid, object.metadata.uid.as_deref()) {
                            return detail;
                        }
                        crashed_container(&object)
                    }
                    Err(error) => {
                        return classify("pods", &error);
                    }
                };
                let tail = match tail_with(&api, &pod, crashed.as_ref()).await {
                    Ok(tail) => tail,
                    Err(detail) => {
                        return detail;
                    }
                };
                match verify_tail_uid(&client, &api, &pod, &uid).await {
                    Ok(()) => InspectDetail::Log(tail),
                    Err(detail) => detail,
                }
            },
            reply,
        )
    }

    fn submit(
        &self,
        work: impl Future<Output = InspectDetail> + Send + 'static,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) -> RequestStop {
        let deadline = tokio::time::Instant::now() + crate::connect::INTERACTIVE_DEADLINE;
        self.scheduler.submit_until(
            &self.handle,
            deadline,
            async move { Fetched::Ok(work.await) },
            move |outcome| {
                reply(match outcome {
                    Fetched::Ok(detail) => detail,
                    Fetched::Denied { what } => InspectDetail::Denied { what },
                    Fetched::Failed { what, why } => InspectDetail::Failed { what, why },
                });
            },
        )
    }
}

fn check_uid(pod: &str, selected: &str, actual: Option<&str>) -> Result<(), InspectDetail> {
    let why = match actual.filter(|uid| !uid.is_empty()) {
        Some(uid) if uid == selected => return Ok(()),
        Some(_) => format!("Pod {pod} no longer has the selected UID; its log tail is withheld"),
        None => format!("Pod {pod} has no UID; its log tail cannot be scoped"),
    };
    Err(InspectDetail::Failed { what: "logs", why })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TailIdentity {
    api_version: String,
    kind: String,
    metadata: TailMetadata,
}

#[derive(Deserialize)]
struct TailMetadata {
    uid: Option<String>,
}

async fn verify_tail_uid(
    client: &Client,
    api: &Api<Pod>,
    pod: &str,
    uid: &str,
) -> Result<(), InspectDetail> {
    let request = Request::new(api.resource_url())
        .get_metadata(pod, &GetParams::default())
        .map_err(|error| classify("pods", &kube::Error::BuildRequest(error)))?;
    let bytes = match crate::reach::bounded_request(client, "pods", request).await {
        Fetched::Ok(bytes) => bytes,
        Fetched::Denied { what } => return Err(InspectDetail::Denied { what }),
        Fetched::Failed { what, why } => return Err(InspectDetail::Failed { what, why }),
    };
    let identity: TailIdentity =
        serde_json::from_slice(&bytes).map_err(|error| InspectDetail::Failed {
            what: "pods",
            why: format!("Pod identity returned an unsupported response: {error}"),
        })?;
    if identity.api_version != "meta.k8s.io/v1" || identity.kind != "PartialObjectMetadata" {
        return Err(InspectDetail::Failed {
            what: "pods",
            why: "Pod identity did not return PartialObjectMetadata; its log tail is withheld"
                .to_string(),
        });
    }
    check_uid(pod, uid, identity.metadata.uid.as_deref())
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
            previous_container_id: last.and_then(|terminated| terminated.container_id.clone()),
            why,
        })
    })
}

struct CrashedContainer {
    name: String,
    previous_container_id: Option<String>,
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
) -> Result<LogTail, InspectDetail> {
    let Some(crashed) = crashed else {
        let stream = api
            .log_stream(pod, &log_params(None, false))
            .await
            .map_err(|error| classify("logs", &error))?;
        let text = read_tail_body(stream).await?;
        return checked_tail(&text, None, false);
    };
    let refusal = match api
        .log_stream(pod, &log_params(Some(&crashed.name), true))
        .await
    {
        Ok(stream) => {
            let text = read_tail_body(stream).await?;
            if !previous_runtime_unavailable(&text, crashed.previous_container_id.as_deref()) {
                return checked_tail(&text, Some(crashed.why.clone()), false);
            }
            text
        }
        Err(error) if previous_unavailable(&error, pod, &crashed.name) => server_sentence(&error),
        Err(error) => return Err(classify("logs", &error)),
    };
    let stream = api
        .log_stream(pod, &log_params(Some(&crashed.name), false))
        .await
        .map_err(|error| classify("logs", &error))?;
    let text = read_tail_body(stream).await?;
    checked_tail(
        &text,
        Some(format!(
            "current container {}; {}; previous log unavailable: {}",
            crashed.name, crashed.why, refusal
        )),
        true,
    )
}

async fn read_tail_body(stream: impl AsyncRead) -> Result<String, InspectDetail> {
    futures::pin_mut!(stream);
    let mut reader = stream.take(LOG_LIMIT_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| InspectDetail::Failed {
            what: "logs",
            why: crate::connect::describe(&error),
        })?;
    if bytes.len() > LOG_LIMIT_BYTES as usize {
        return Err(InspectDetail::Failed {
            what: "logs",
            why: format!(
                "logs answered with more than {LOG_LIMIT_BYTES} bytes; the body is refused"
            ),
        });
    }
    String::from_utf8(bytes).map_err(|error| classify("logs", &kube::Error::FromUtf8(error)))
}

fn checked_tail(
    text: &str,
    note: Option<String>,
    previous_unavailable: bool,
) -> Result<LogTail, InspectDetail> {
    // The CRI reader prefixes the first application chunk when timestamps are
    // requested. A timestamp-free body can be a refusal after headers flushed.
    if !text.is_empty()
        && !text
            .split_once(' ')
            .is_some_and(|(timestamp, _)| timestamp.parse::<k8s_openapi::jiff::Timestamp>().is_ok())
    {
        return Err(InspectDetail::Failed {
            what: "logs",
            why: text.to_string(),
        });
    }
    Ok(LogTail {
        lines: tail_lines(text),
        note,
        previous_unavailable,
    })
}

fn previous_runtime_unavailable(text: &str, container_id: Option<&str>) -> bool {
    container_id.is_some_and(|container_id| {
        container_id
            .split_once("://")
            .is_some_and(|(runtime, id)| !runtime.is_empty() && !id.is_empty())
            && text.strip_prefix(RUNTIME_LOG_REFUSAL) == Some(container_id)
    })
}

fn previous_unavailable(error: &kube::Error, pod: &str, container: &str) -> bool {
    matches!(error, kube::Error::Api(status)
        if status.code == 400
            && status.reason == "BadRequest"
            && status.message.trim()
                == format!("previous terminated container \"{container}\" in pod \"{pod}\" not found"))
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
        why: server_sentence(error),
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
