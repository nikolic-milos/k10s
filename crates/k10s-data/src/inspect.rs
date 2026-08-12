use std::sync::Arc;

use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{ListParams, LogParams};
use kube::{Api, Client};

use crate::connect::describe;

// One-shot reads for the inspector panel: recent events for an object and a
// bounded log tail. Fire-and-forget onto the data plane's runtime, the reply
// handed to a caller-supplied callback -- the render thread never blocks on
// the cluster, and a denial arrives as a labelled variant, never an empty
// panel.
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
                    let mut lines: Vec<EventLine> = list
                        .items
                        .into_iter()
                        .map(|event| EventLine {
                            last_seen: event
                                .last_timestamp
                                .map(|t| t.0.to_string())
                                .or_else(|| event.event_time.map(|t| t.0.to_string()))
                                .unwrap_or_default(),
                            kind: event.type_.unwrap_or_default(),
                            reason: event.reason.unwrap_or_default(),
                            message: event.message.unwrap_or_default(),
                            count: event.count.unwrap_or(1),
                        })
                        .collect();
                    lines.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                    lines.truncate(MAX_EVENTS);
                    InspectDetail::Events(lines)
                }
                Err(error) => classify("events", &error),
            };
            reply(outcome);
        });
    }

    pub fn fetch_log_tail(
        &self,
        namespace: &str,
        pod: &Arc<str>,
        reply: impl FnOnce(InspectDetail) + Send + 'static,
    ) {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = pod.clone();
        let params = LogParams {
            tail_lines: Some(LOG_TAIL_LINES),
            limit_bytes: Some(LOG_LIMIT_BYTES),
            timestamps: true,
            ..LogParams::default()
        };
        self.handle.spawn(async move {
            let outcome = match api.logs(&pod, &params).await {
                Ok(text) => InspectDetail::Log(LogTail {
                    lines: text.lines().map(str::to_owned).collect(),
                }),
                Err(error) => classify("logs", &error),
            };
            reply(outcome);
        });
    }
}

fn classify(what: &'static str, error: &kube::Error) -> InspectDetail {
    if let kube::Error::Api(response) = error
        && response.code == 403
    {
        return InspectDetail::Denied { what };
    }
    InspectDetail::Failed {
        what,
        why: describe(error as &(dyn std::error::Error + 'static)),
    }
}
