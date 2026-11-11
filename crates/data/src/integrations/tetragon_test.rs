//! Field extraction, caps, 404/403, both group names, hook-count math,
//! the event parser, and a planted kprobe arg filter that must not appear
//! in Debug. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

const PLANTED: &str = "planted-kprobe-arg-secret-7f3a";

fn policy_json() -> Value {
    json!({
        "metadata": {
            "name": "sys-write",
            "uid": "tp-1",
            "annotations": {}
        },
        "spec": {
            "disabled": false,
            "podSelector": { "matchLabels": { "app": "xwing", "tier": "prod" } },
            "kprobes": [
                {
                    "call": "sys_write",
                    "syscall": true,
                    "selectors": [{
                        "matchArgs": [{
                            "index": 1,
                            "operator": "Equal",
                            "values": [PLANTED]
                        }]
                    }]
                },
                { "call": "sys_read", "syscall": true }
            ],
            "lsmhooks": [{ "hook": "file_open" }],
            "tracepoints": [
                { "subsystem": "raw_syscalls", "event": "sys_enter" },
                { "subsystem": "raw_syscalls", "event": "sys_exit" },
                { "subsystem": "sched", "event": "sched_process_exec" }
            ],
            "uprobes": [
                { "path": "/bin/bash", "symbols": ["readline"] },
                { "path": "/bin/bash", "symbols": ["execute_command"] },
                { "path": "/usr/bin/ssh", "symbols": ["main"] },
                { "path": "/usr/bin/curl", "symbols": ["main"] }
            ]
        },
        "status": { "state": "enabled" }
    })
}

fn namespaced_json() -> Value {
    json!({
        "metadata": { "name": "ns-lseek", "namespace": "default", "uid": "tpn-1" },
        "spec": {
            "disabled": true,
            "kprobes": [{ "call": "sys_lseek", "syscall": true }],
            "podSelector": {
                "matchExpressions": [{
                    "key": "env",
                    "operator": "In",
                    "values": ["prod", "staging"]
                }]
            }
        },
        "status": { "error": "load failed on node-a" }
    })
}

fn podinfo_json() -> Value {
    json!({
        "metadata": {
            "name": "xwing",
            "namespace": "default",
            "uid": "podinfo-uid",
            "ownerReferences": [{ "kind": "Pod", "name": "xwing", "uid": "pod-uid-9" }]
        },
        "workloadType": { "kind": "Deployment", "apiVersion": "apps/v1" },
        "workloadObject": { "name": "xwing", "namespace": "default" }
    })
}

fn policy_from(kind: Kind, group: &str, value: Value) -> DeclaredPolicy {
    parse_policy(kind, group, VERSION, value).expect("the fixture is a TracingPolicy")
}

#[test]
fn hook_counts_are_lengths_without_holding_bodies() {
    let policy = policy_from(Kind::TracingPolicy, TETRAGON_GROUP, policy_json());
    assert_eq!(policy.kprobes, 2);
    assert_eq!(policy.lsm, 1);
    assert_eq!(policy.tracepoints, 3);
    assert_eq!(policy.uprobes, 4);
    assert_eq!(policy.pod_selector, "app=xwing,tier=prod");
    assert!(policy.enabled);
    assert_eq!(policy.status, "enabled");
}

#[test]
fn missing_hook_arrays_count_as_zero() {
    let policy = policy_from(
        Kind::TracingPolicy,
        CILIUM_GROUP,
        json!({
            "metadata": { "name": "empty" },
            "spec": { "kprobes": "not-an-array" }
        }),
    );
    assert_eq!(policy.kprobes, 0);
    assert_eq!(policy.lsm, 0);
    assert_eq!(policy.tracepoints, 0);
    assert_eq!(policy.uprobes, 0);
}

#[test]
fn a_planted_kprobe_arg_filter_does_not_leak_into_debug_or_table_cells() {
    let policy = policy_from(Kind::TracingPolicy, TETRAGON_GROUP, policy_json());
    let inventory = Inventory {
        tetragon: GroupState::Served,
        tracing_policies: KindSet::Served {
            items: vec![policy.clone()],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let page = table_page(&inventory).expect("a served policy is a table");
    let debug = format!("{policy:?} {inventory:?} {page:?}");
    assert!(
        !debug.contains(PLANTED),
        "a kprobe arg filter must not be stored: {debug}"
    );
    for row in &page.rows {
        for cell in &row.cells {
            assert!(
                !cell.contains(PLANTED),
                "table cell leaked {PLANTED}: {cell}"
            );
        }
    }
}

#[test]
fn namespaced_policy_uses_object_namespace_and_spec_disabled() {
    let policy = policy_from(
        Kind::TracingPolicyNamespaced,
        CILIUM_GROUP,
        namespaced_json(),
    );
    assert!(!policy.enabled);
    assert_eq!(policy.namespace, "default");
    assert_eq!(policy.scope_selector, "ns default");
    assert_eq!(policy.pod_selector, "env In (prod,staging)");
    assert_eq!(policy.status, "load failed on node-a");
    assert_eq!(policy.kprobes, 1);
}

#[test]
fn scope_selector_labels_container_then_node_then_host_and_ignores_namespace_selector() {
    let scoped = |spec: Value| {
        policy_from(
            Kind::TracingPolicy,
            TETRAGON_GROUP,
            json!({ "metadata": { "name": "scoped" }, "spec": spec }),
        )
    };
    let container = scoped(json!({
        "containerSelector": { "matchLabels": { "app": "xwing" } },
        "nodeSelector": { "matchLabels": { "role": "worker" } }
    }));
    assert_eq!(container.scope_selector, "container app=xwing");
    let node = scoped(json!({
        "nodeSelector": { "matchLabels": { "role": "worker" } },
        "hostSelector": { "matchLabels": { "kind": "bare" } }
    }));
    assert_eq!(node.scope_selector, "node role=worker");
    let host = scoped(json!({ "hostSelector": { "matchLabels": { "kind": "bare" } } }));
    assert_eq!(host.scope_selector, "host kind=bare");
    // TracingPolicySpec has no namespaceSelector; reading one would render a
    // field no real Tetragon ever serves.
    let phantom = scoped(json!({
        "namespaceSelector": { "matchLabels": { "team": "sre" } }
    }));
    assert_eq!(phantom.scope_selector, "");
    assert_eq!(selector_cell(&phantom), "");
}

#[test]
fn a_policy_without_status_says_the_crd_has_none_and_stays_enabled() {
    let policy = policy_from(
        Kind::TracingPolicy,
        TETRAGON_GROUP,
        json!({
            "metadata": { "name": "bare" },
            "spec": { "kprobes": [{ "call": "sys_write" }] }
        }),
    );
    assert_eq!(
        policy.status, STATUS_ABSENT,
        "upstream TracingPolicy is +genclient:noStatus; a blank cell would read as healthy"
    );
    assert!(policy.enabled);
}

#[test]
fn a_grpc_tp_state_disabled_string_is_not_a_disabled_row() {
    let policy = policy_from(
        Kind::TracingPolicy,
        TETRAGON_GROUP,
        json!({
            "metadata": { "name": "grpc-shaped" },
            "spec": {},
            "status": { "state": "TP_STATE_DISABLED" }
        }),
    );
    assert!(
        policy.enabled,
        "TP_STATE_DISABLED is a gRPC enum value no CRD stores"
    );
    let fork = policy_from(
        Kind::TracingPolicy,
        TETRAGON_GROUP,
        json!({
            "metadata": { "name": "forked" },
            "spec": {},
            "status": { "state": "disabled" }
        }),
    );
    assert!(
        !fork.enabled,
        "a fork that writes a status is still honoured"
    );
}

#[test]
fn podinfo_keeps_pod_uid_and_workload_without_a_process_dump() {
    let info = parse_podinfo(TETRAGON_GROUP, VERSION, podinfo_json()).expect("podinfo");
    assert_eq!(info.name, "xwing");
    assert_eq!(info.namespace, "default");
    assert_eq!(info.pod_uid, "pod-uid-9");
    assert_eq!(info.workload, "Deployment/default/xwing");
    let debug = format!("{info:?}");
    assert!(!debug.contains("process"), "{debug}");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_policy(Kind::TracingPolicy, TETRAGON_GROUP, VERSION, json!({})).is_none());
    assert!(parse_podinfo(TETRAGON_GROUP, VERSION, json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": huge, "uid": huge },
        "spec": {
            "podSelector": { "matchLabels": { "app": huge } },
            "status": { "error": huge }
        },
        "status": { "error": huge }
    });
    let policy = policy_from(Kind::TracingPolicy, TETRAGON_GROUP, value);
    for field in [
        &policy.name,
        &policy.namespace,
        &policy.uid,
        &policy.pod_selector,
        &policy.status,
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like Tetragon is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn table_page_is_none_when_neither_tetragon_kind_is_served() {
    let empty = Inventory::default();
    assert!(table_page(&empty).is_none());
    let cilium_only = Inventory {
        cilium: GroupState::Served,
        ..Inventory::default()
    };
    assert!(
        table_page(&cilium_only).is_none(),
        "cilium.io answering for CNP is not a Tetragon table"
    );
}

#[test]
fn table_page_is_some_when_a_kind_is_served_or_a_group_is_denied() {
    let empty_list = Inventory {
        tetragon: GroupState::Served,
        tracing_policies: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let page = table_page(&empty_list).expect("CRDs with zero objects are a table");
    assert!(page.rows.is_empty());

    let denied = Inventory {
        cilium: GroupState::Denied,
        tracing_policies: KindSet::Denied,
        tracing_policies_namespaced: KindSet::Denied,
        pod_infos: KindSet::Denied,
        ..Inventory::default()
    };
    let page = table_page(&denied).expect("403 is a labelled table, not absence");
    assert!(
        page.rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| cell.contains("access denied"))),
        "{page:?}"
    );
}

#[test]
fn grpc_events_stay_unbound() {
    assert_eq!(
        event_source(),
        EventSource::Unbound {
            why: EVENTS_UNBOUND_WHY
        }
    );
}

fn exec_event() -> &'static str {
    r#"{
      "process_exec": {
        "process": {
          "exec_id": "Z2tl",
          "pid": 52699,
          "binary": "/usr/bin/curl",
          "arguments": "https://ebpf.io/applications/#tetragon",
          "flags": "execve rootcwd",
          "pod": { "namespace": "default", "name": "xwing" }
        }
      },
      "node_name": "kind-control-plane",
      "time": "2023-10-06T22:03:57.700327580Z"
    }"#
}

fn kprobe_event() -> &'static str {
    r#"{
      "process_kprobe": {
        "process": {
          "binary": "/usr/sbin/sshd",
          "flags": "procFS",
          "pod": { "namespace": "kube-system", "name": "sshd" }
        },
        "function_name": "fd_install",
        "args": [
          { "file_arg": { "path": "/etc/shadow" } },
          { "string_arg": "fd-install" }
        ],
        "policy_name": "fd-install"
      }
    }"#
}

#[test]
fn parse_events_reads_process_exec_and_process_kprobe() {
    let exec = parse_events(exec_event().as_bytes()).expect("exec json");
    assert_eq!(exec.events.len(), 1);
    assert_eq!(exec.events[0].kind, ObservedKind::ProcessExec);
    assert_eq!(exec.events[0].names, "default/xwing");
    assert_eq!(exec.events[0].binary, "/usr/bin/curl");
    assert_eq!(exec.events[0].flags, "execve rootcwd");
    assert!(exec.events[0].args.contains("ebpf.io"));

    let kprobe = parse_events(kprobe_event().as_bytes()).expect("kprobe json");
    assert_eq!(kprobe.events[0].kind, ObservedKind::ProcessKprobe);
    assert_eq!(kprobe.events[0].names, "kube-system/sshd");
    assert_eq!(kprobe.events[0].binary, "/usr/sbin/sshd");
    assert_eq!(kprobe.events[0].flags, "procFS");
    assert!(kprobe.events[0].args.contains("/etc/shadow"));
}

#[test]
fn parse_events_accepts_proto_json_camel_case_and_ndjson() {
    let camel = br#"{"processExec":{"process":{"binary":"/bin/ls","flags":"execve","pod":{"namespace":"kube-system","name":"coredns"}}}}"#;
    let parsed = parse_events(camel).expect("camelCase");
    assert_eq!(parsed.events[0].kind, ObservedKind::ProcessExec);
    assert_eq!(parsed.events[0].binary, "/bin/ls");
    assert_eq!(parsed.events[0].names, "kube-system/coredns");

    let ndjson = format!(
        "{}\n{}\n",
        serde_json::to_string(&serde_json::from_str::<serde_json::Value>(exec_event()).unwrap())
            .unwrap(),
        serde_json::to_string(&serde_json::from_str::<serde_json::Value>(kprobe_event()).unwrap())
            .unwrap()
    );
    let parsed = parse_events(ndjson.as_bytes()).expect("ndjson");
    assert_eq!(parsed.events.len(), 2);
    assert_eq!(parsed.events[0].kind, ObservedKind::ProcessExec);
    assert_eq!(parsed.events[1].kind, ObservedKind::ProcessKprobe);
}

#[test]
fn parse_events_caps_count_and_arg_bytes() {
    let huge = "x".repeat(MAX_ARG_BYTES + 80);
    let one = json!({
        "process_exec": {
            "process": { "binary": "/bin/sh", "flags": "execve", "arguments": huge }
        }
    });
    let parsed = parse_events(one.to_string().as_bytes()).expect("one");
    assert!(parsed.events[0].args.len() <= MAX_ARG_BYTES + 3);
    assert!(parsed.events[0].args.ends_with('\u{2026}'));

    let many: Vec<Value> = (0..MAX_EVENTS + 5)
        .map(|i| {
            json!({
                "process_exec": {
                    "process": { "binary": format!("/bin/{i}"), "flags": "execve" }
                }
            })
        })
        .collect();
    let parsed = parse_events(Value::Array(many).to_string().as_bytes()).expect("many");
    assert_eq!(parsed.events.len(), MAX_EVENTS);
    assert!(parsed.truncated);

    let too_big = vec![b'x'; MAX_PAGE_BYTES + 1];
    assert!(matches!(
        parse_events(&too_big),
        Err(EventsError::TooLarge { .. })
    ));
}

#[test]
fn observed_events_are_not_declared_policies() {
    let events = parse_events(exec_event().as_bytes()).expect("exec");
    let policy = policy_from(Kind::TracingPolicy, TETRAGON_GROUP, policy_json());
    assert_ne!(
        format!("{:?}", events.events[0].kind),
        format!("{:?}", policy.kind)
    );
}

#[test]
fn workload_fingerprint_is_exact_name_or_label() {
    let mut labels = BTreeMap::new();
    assert!(matches_workload("tetragon", &labels));
    assert!(matches_workload("cilium-tetragon", &labels));
    assert!(matches_workload("Tetragon", &labels));
    assert!(!matches_workload("tetragon-operator", &labels));
    assert!(!matches_workload("nginx", &labels));
    labels.insert(WORKLOAD_LABEL.to_string(), "tetragon".to_string());
    assert!(matches_workload("agent", &labels));
    labels.insert(WORKLOAD_LABEL.to_string(), "cilium".to_string());
    assert!(!matches_workload("agent", &labels));
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
}

struct Route {
    method: &'static str,
    matches: String,
    status: u16,
    body: String,
    used: bool,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    seen: Vec<Seen>,
}

#[derive(Clone, Default)]
struct Script {
    state: Arc<Mutex<State>>,
}

impl Script {
    fn route(
        &self,
        method: &'static str,
        matches: &str,
        status: u16,
        body: impl Into<String>,
    ) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            status,
            body: body.into(),
            used: false,
        });
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.state.lock().expect("script lock").seen.clone()
    }

    fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }
}

impl Service<http::Request<Body>> for Script {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();
        let answer = {
            let mut state = self.state.lock().expect("script lock");
            state.seen.push(Seen {
                method: method.clone(),
                path: path.clone(),
            });
            let routable = path.replacen("?&", "?", 1);
            let hit = state.routes.iter_mut().find(|route| {
                !route.used && route.method == method && routable.starts_with(&route.matches)
            });
            let answer = match hit {
                Some(route) => {
                    route.used = true;
                    Some((route.status, route.body.clone()))
                }
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            };
            answer
        };
        let body = req.into_body();
        Box::pin(async move {
            let _ = http_body_util::BodyExt::collect(body).await;
            let (status, response) = answer.expect("every scripted call answers");
            Ok(http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(response.into_bytes()))
                .expect("a response"))
        })
    }
}

const STATUS_403: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"forbidden"}"#;

fn group_doc(group: &str, version: &str) -> String {
    format!(
        r#"{{"kind":"APIGroup","name":"{group}","versions":[{{"groupVersion":"{group}/{version}","version":"{version}"}}],"preferredVersion":{{"groupVersion":"{group}/{version}","version":"{version}"}}}}"#
    )
}

fn list(items: &[Value]) -> String {
    json!({ "kind": "List", "metadata": {}, "items": items }).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_both_groups_is_invisible_and_does_not_list_policies() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("a missing group is not a failure: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.cilium, GroupState::NotServed));
    assert!(matches!(inventory.tetragon, GroupState::NotServed));
    assert!(matches!(inventory.tracing_policies, KindSet::NotServed));
    assert!(table_page(&inventory).is_none());
    assert!(
        script
            .seen()
            .iter()
            .any(|seen| seen.path == "/apis/cilium.io"),
        "cilium.io must be probed: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .any(|seen| seen.path == "/apis/tetragon.io"),
        "tetragon.io must be probed: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("tracingpolicies")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.seen().iter().all(|seen| seen.method == "GET"),
        "a Tetragon fetch only reads: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_a_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/tetragon.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied, not a whole-fetch failure");
    };
    assert!(matches!(inventory.tetragon, GroupState::Denied));
    assert!(inventory.served(), "403 is visible, not served: false");
    assert!(matches!(inventory.tracing_policies, KindSet::Denied));
    assert!(table_page(&inventory).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tetragon_io_serves_tracingpolicies() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/tetragon.io",
        200,
        group_doc(TETRAGON_GROUP, VERSION),
    );
    script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpolicies?",
        200,
        list(&[policy_json()]),
    );
    script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpoliciesnamespaced?",
        200,
        list(&[]),
    );
    script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/podinfo?",
        200,
        list(&[podinfo_json()]),
    );
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    assert!(matches!(inventory.tetragon, GroupState::Served));
    let policy = &inventory.tracing_policies.items()[0];
    assert_eq!(policy.name, "sys-write");
    assert_eq!(policy.group, TETRAGON_GROUP);
    assert_eq!(policy.kprobes, 2);
    assert_eq!(inventory.pod_infos.items()[0].pod_uid, "pod-uid-9");
    let page = table_page(&inventory).expect("served kinds are a table");
    assert!(page.rows.iter().any(|row| row.name == "sys-write"));
    let debug = format!("{inventory:?}");
    assert!(!debug.contains(PLANTED), "{debug}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cilium_io_still_serves_legacy_tracingpolicies() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/cilium.io",
        200,
        group_doc(CILIUM_GROUP, VERSION),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v1alpha1/tracingpolicies?",
        200,
        list(&[policy_json()]),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v1alpha1/tracingpoliciesnamespaced?",
        200,
        list(&[]),
    );
    script.route("GET", "/apis/cilium.io/v1alpha1/podinfo?", 200, list(&[]));
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("legacy cilium.io listing must resolve");
    };
    assert!(matches!(inventory.cilium, GroupState::Served));
    assert_eq!(inventory.tracing_policies.items()[0].group, CILIUM_GROUP);
    assert!(table_page(&inventory).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cilium_io_v2_without_tracingpolicy_is_not_a_table() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc(CILIUM_GROUP, "v2"));
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("CNP-only cilium.io is Ok, not Failed: {fetched:?}");
    };
    assert!(matches!(inventory.cilium, GroupState::Served));
    assert!(matches!(inventory.tracing_policies, KindSet::NotServed));
    assert!(
        table_page(&inventory).is_none(),
        "CiliumNetworkPolicy on cilium.io is not Tetragon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tetragon_daemonset_is_a_workload_fingerprint() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        list(&[json!({
            "metadata": {
                "name": "tetragon",
                "namespace": "kube-system",
                "labels": { "app.kubernetes.io/name": "tetragon" }
            }
        })]),
    );
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("workload listing must resolve");
    };
    let WorkloadSet::Found(ref found) = inventory.workload else {
        panic!("the DaemonSet is the fingerprint: {:?}", inventory.workload);
    };
    assert_eq!(found[0].name, "tetragon");
    assert_eq!(found[0].kind, WorkloadKind::DaemonSet);
    assert!(
        table_page(&inventory).is_none(),
        "a workload without CRDs is not a TracingPolicy table"
    );
}

#[test]
fn a_denied_group_alone_keeps_tetragon_visible() {
    let denied_cilium = Inventory {
        cilium: GroupState::Denied,
        ..Inventory::default()
    };
    assert!(
        denied_cilium.served(),
        "a denied cilium.io group means absence cannot be claimed"
    );
    let denied_tetragon = Inventory {
        tetragon: GroupState::Denied,
        ..Inventory::default()
    };
    assert!(
        denied_tetragon.served(),
        "a denied tetragon.io group means absence cannot be claimed"
    );
}
