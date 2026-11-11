//! Parser fixtures from real Falco JSON, field caps, 404/403, and the
//! rule-ConfigMap leak: a planted token in an output string must not
//! appear in Debug.

use super::*;
use crate::read::Fetched;
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use kube::client::Body;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service as TowerService;

const PLANTED: &str = "PLANTED_FALCO_RULE_TOKEN_9f3a";

/// Published Falco JSON from the outputs documentation (Terminal shell
/// in container). Command line and host-adjacent fields stay in the
/// fixture so the parser can prove it dropped them.
fn terminal_shell_json() -> &'static str {
    r#"{"hostname":"falco-xczjd","output":"13:44:05.478445995: Critical A shell was spawned in a container with an attached terminal (user=root user_loginuid=-1 k8s.ns=default k8s.pod=kubecon container=ee97d9c4186f shell=sh parent=runc cmdline=sh -c clear; (bash || ash || sh) terminal=34816 container_id=ee97d9c4186f image=docker.io/library/alpine)","priority":"Critical","rule":"Terminal shell in container","source":"syscall","tags":["container","mitre_execution","shell"],"time":"2023-05-25T13:44:05.478445995Z","output_fields":{"container.id":"ee97d9c4186f","container.image.repository":"docker.io/library/alpine","evt.time":1685022245478445995,"k8s.ns.name":"default","k8s.pod.name":"kubecon","proc.cmdline":"sh -c clear; (bash || ash || sh)","proc.name":"sh","proc.pname":"runc","proc.tty":34816,"user.loginuid":-1,"user.name":"root"}}"#
}

fn write_below_binary_json() -> &'static str {
    r#"{"hostname":"ip-10-0-0-76.us-west-2.compute.internal","output":"10:20:05.211321183: Warning File below a known binary directory opened for writing (user=root command=touch /bin/foo file=/bin/foo)","output_fields":{"evt.time":1507021205211321183,"fd.name":"/bin/foo","proc.cmdline":"touch /bin/foo","user.name":"root","k8s.ns.name":"kube-system","k8s.pod.name":"coredns-abc"},"priority":"Warning","rule":"Write below binary dir","source":"syscall","tags":["filesystem","mitre_persistence"],"time":"2017-10-03T10:20:05.211321183Z"}"#
}

fn planted_event_json() -> String {
    format!(
        r#"{{"priority":"Debug","rule":"Planted output","time":"2024-01-01T00:00:00.000000000Z","output":"host path /etc/shadow and {PLANTED} cmdline","output_fields":{{"k8s.ns.name":"prod","k8s.pod.name":"api","proc.cmdline":"{PLANTED} /bin/sh","fd.name":"/etc/shadow"}}}}"#
    )
}

#[test]
fn a_real_falco_json_line_keeps_priority_rule_and_pod() {
    let events = parse_log_chunk(terminal_shell_json());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].priority, "Critical");
    assert_eq!(events[0].rule, "Terminal shell in container");
    assert_eq!(events[0].namespace, "default");
    assert_eq!(events[0].pod, "kubecon");
    assert_eq!(events[0].time, "2023-05-25T13:44:05.478445995Z");
}

#[test]
fn a_write_below_binary_dir_event_keeps_namespace_and_drops_the_host_path() {
    let events = parse_log_chunk(write_below_binary_json());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].priority, "Warning");
    assert_eq!(events[0].rule, "Write below binary dir");
    assert_eq!(events[0].namespace, "kube-system");
    assert_eq!(events[0].pod, "coredns-abc");
    let debug = format!("{:?}", events[0]);
    assert!(
        !debug.contains("/bin/foo"),
        "host path survived on the event: {debug}"
    );
    assert!(
        !debug.contains("touch"),
        "command line survived on the event: {debug}"
    );
}

#[test]
fn a_kubelet_timestamp_prefix_is_stripped_before_json() {
    let line = format!("2023-05-25T13:44:05.478445995Z {}", terminal_shell_json());
    let events = parse_log_chunk(&line);
    assert_eq!(events[0].rule, "Terminal shell in container");
}

#[test]
fn a_json_array_chunk_is_parsed() {
    let chunk = format!("[{},{}]", terminal_shell_json(), write_below_binary_json());
    let events = parse_log_chunk(&chunk);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].rule, "Terminal shell in container");
    assert_eq!(events[1].rule, "Write below binary dir");
}

#[test]
fn a_numeric_priority_uses_falco_names() {
    let events = parse_log_chunk(r#"{"rule":"n","priority":2,"time":"t"}"#);
    assert_eq!(events[0].priority, "Critical");
}

#[test]
fn planted_tokens_in_output_and_syscall_args_do_not_appear_in_debug() {
    let events = parse_log_chunk(&planted_event_json());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].rule, "Planted output");
    assert_eq!(events[0].namespace, "prod");
    let debug = format!("{events:?}");
    assert!(
        !debug.contains(PLANTED),
        "planted token survived Debug: {debug}"
    );
    assert!(
        !debug.contains("/etc/shadow"),
        "host path survived Debug: {debug}"
    );
    assert!(!debug.contains("cmdline"), "{debug}");
}

#[test]
fn parse_log_chunk_stops_at_the_event_cap() {
    let mut chunk = String::new();
    for i in 0..(MAX_EVENTS + 40) {
        chunk.push_str(&format!(
            r#"{{"rule":"r{i}","priority":"Warning","time":"t{i}","output_fields":{{"k8s.ns.name":"ns","k8s.pod.name":"p"}}}}"#
        ));
        chunk.push('\n');
    }
    let events = parse_log_chunk(&chunk);
    assert_eq!(events.len(), MAX_EVENTS);
}

#[test]
fn an_enormous_rule_name_is_clipped() {
    let huge = "r".repeat(6 << 10);
    let value = json!({
        "rule": huge,
        "priority": "Warning",
        "time": huge,
        "output_fields": {
            "k8s.ns.name": huge,
            "k8s.pod.name": huge
        }
    });
    let event = parse_event(&value).expect("a named rule is an event");
    assert!(event.rule.chars().count() <= MAX_FIELD_CHARS + 1);
    assert!(event.time.chars().count() <= MAX_FIELD_CHARS + 1);
    assert!(event.namespace.chars().count() <= MAX_FIELD_CHARS + 1);
    assert!(event.pod.chars().count() <= MAX_FIELD_CHARS + 1);
}

#[test]
fn count_rules_counts_yaml_entries_and_ignores_macros() {
    let text = "\
- macro: never_true
  condition: (evt.num=0)
- list: shells
  items: [bash, sh]
- rule: Terminal shell in container
  desc: a shell
  condition: spawned_process
  output: A shell was spawned (cmdline=%proc.cmdline)
  priority: WARNING
- rule: Write below binary dir
  condition: open_write
  output: File opened (file=%fd.name)
  priority: ERROR
";
    assert_eq!(count_rules(text), 2);
}

#[test]
fn a_falco_cr_keeps_version_ready_and_rules_file_names() {
    // The operator publishes `Reconciled`/`Available` on instance kinds;
    // `Available` wins regardless of the array order.
    let value = json!({
        "kind": "Falco",
        "metadata": {"name": "falco", "namespace": "falco", "uid": "cr-1"},
        "spec": {
            "version": "0.40.0",
            "configMapRef": {"name": "extra-rules"},
            "rulesFiles": [{"name": "falco-rules"}, "custom-rules"]
        },
        "status": {"conditions": [
            {"type": "Reconciled", "status": "True"},
            {"type": "Available", "status": "True"}
        ]}
    });
    let resource = parse_resource(
        CrKind::Falco,
        "instance.falcosecurity.dev",
        "v1alpha1",
        &value,
    )
    .expect("a named Falco CR");
    assert_eq!(resource.name, "falco");
    assert_eq!(resource.namespace, "falco");
    assert_eq!(resource.image, "0.40.0");
    assert_eq!(resource.ready, "Available=True");
    assert_eq!(
        resource.rules_refs,
        vec!["extra-rules", "falco-rules", "custom-rules"]
    );
}

#[test]
fn a_legacy_chart_ready_condition_is_read_when_operator_conditions_are_absent() {
    let value = json!({
        "kind": "Falco",
        "metadata": {"name": "falco", "namespace": "falco"},
        "status": {"conditions": [{"type": "Ready", "status": "True"}]}
    });
    let resource =
        parse_resource(CrKind::Falco, "falco.org", "v1alpha1", &value).expect("a named Falco CR");
    assert_eq!(resource.ready, "Ready=True");
}

#[test]
fn a_rulesfile_cr_uses_the_oci_image_and_configmap_name() {
    let value = json!({
        "kind": "Rulesfile",
        "metadata": {"name": "falco-rules", "namespace": "falco"},
        "spec": {
            "ociArtifact": {
                "image": {
                    "repository": "falcosecurity/rules/falco-rules",
                    "tag": "3.2.0"
                }
            },
            "configMapRef": {"name": "inline-extra"},
            "inlineRules": "- rule: planted\n  output: should not be a ref\n"
        },
        "status": {"conditions": [
            {"type": "ResolvedRefs", "status": "True"},
            {"type": "Programmed", "status": "False"}
        ]}
    });
    let resource = parse_resource(
        CrKind::FalcoRules,
        "artifact.falcosecurity.dev",
        "v1alpha1",
        &value,
    )
    .expect("a named Rulesfile");
    assert_eq!(resource.kind, CrKind::FalcoRules);
    assert_eq!(resource.kind_name, "Rulesfile");
    assert_eq!(resource.image, "falcosecurity/rules/falco-rules:3.2.0");
    // Artifact kinds answer `Programmed` first; `ResolvedRefs` is the
    // fallback, not the winner.
    assert_eq!(resource.ready, "Programmed=False");
    assert_eq!(resource.rules_refs, vec!["inline-extra"]);
    let debug = format!("{resource:?}");
    assert!(
        !debug.contains("should not be a ref"),
        "inline rule text survived: {debug}"
    );
}

#[test]
fn a_config_cr_is_classified_and_its_inline_fragment_never_leaves() {
    assert!(matches!(
        classify_kind("Config"),
        Some(ListedKind::Resource(CrKind::FalcoConfig))
    ));
    // `spec.config` is an inline Falco config fragment that can carry
    // output URLs with embedded tokens; only the ConfigMap name may leave.
    let value = json!({
        "kind": "Config",
        "metadata": {"name": "sidekick-config", "namespace": "falco", "uid": "cfg-1"},
        "spec": {
            "config": format!("falcosidekick:\n  webhook:\n    address: https://hook.example/{PLANTED}\n"),
            "configMapRef": {"name": "sidekick-fragment"},
            "priority": 10
        },
        "status": {"conditions": [{"type": "Programmed", "status": "True"}]}
    });
    let resource = parse_resource(
        CrKind::FalcoConfig,
        "artifact.falcosecurity.dev",
        "v1alpha1",
        &value,
    )
    .expect("a named Config CR");
    assert_eq!(resource.kind, CrKind::FalcoConfig);
    assert_eq!(resource.kind_name, "Config");
    assert_eq!(resource.ready, "Programmed=True");
    assert_eq!(resource.rules_refs, vec!["sidekick-fragment"]);
    let debug = format!("{resource:?}");
    assert!(
        !debug.contains(PLANTED),
        "inline config fragment survived Debug: {debug}"
    );
}

#[test]
fn a_denied_cr_list_is_a_labelled_row_not_an_empty_cluster() {
    let mut inventory = Inventory::default();
    inventory.groups[0].1 = GroupState::Served;
    inventory.denied_kinds.push((
        "instance.falcosecurity.dev".to_string(),
        "falcos".to_string(),
    ));
    let page = table_page(&inventory).expect("a served group with a denied kind is visible");
    assert!(
        page.rows
            .iter()
            .any(|row| row.uid == "denied:instance.falcosecurity.dev/falcos"
                && row
                    .cells
                    .iter()
                    .any(|cell| cell == "access denied for this account")),
        "{page:?}"
    );
    let document = render(&inventory).join("\n");
    assert!(
        !document.contains("no Falco objects are stored"),
        "a denied list claimed emptiness: {document}"
    );
    assert!(
        document.contains("instance.falcosecurity.dev/falcos: access denied for this account"),
        "{document}"
    );
}

#[test]
fn a_falco_event_cr_reads_spec_fields_and_drops_output() {
    let value = json!({
        "kind": "FalcoEvent",
        "metadata": {
            "name": "evt-1",
            "namespace": "default",
            "creationTimestamp": "2023-05-25T13:44:05Z"
        },
        "spec": {
            "rule": "Terminal shell in container",
            "priority": "Critical",
            "output": format!("cmdline {PLANTED}"),
            "output_fields": {
                "k8s.ns.name": "default",
                "k8s.pod.name": "kubecon",
                "proc.cmdline": PLANTED
            }
        }
    });
    let event = parse_event(&value).expect("a FalcoEvent CR");
    assert_eq!(event.rule, "Terminal shell in container");
    assert_eq!(event.priority, "Critical");
    assert_eq!(event.namespace, "default");
    assert_eq!(event.pod, "kubecon");
    let debug = format!("{event:?}");
    assert!(
        !debug.contains(PLANTED),
        "CR output survived Debug: {debug}"
    );
}

#[test]
fn table_page_is_none_when_nothing_is_served_and_no_workload_was_found() {
    let inventory = Inventory::default();
    assert!(!inventory.present());
    assert!(table_page(&inventory).is_none());
}

#[test]
fn a_labelled_service_and_named_daemonset_match() {
    let svc: Service = serde_json::from_value(json!({
        "metadata": {
            "name": "alerts",
            "namespace": "falco",
            "uid": "svc-1",
            "labels": {"app.kubernetes.io/name": "falcosidekick"}
        },
        "spec": {"ports": [{"port": 2801}]}
    }))
    .expect("service");
    let found = match_service(&svc).expect("falcosidekick label");
    assert_eq!(found.kind, WorkloadKind::Falcosidekick);
    assert_eq!(found.source, WorkloadSource::Service);

    let ds: DaemonSet = serde_json::from_value(json!({
        "metadata": {"name": "falco", "namespace": "falco", "uid": "ds-1"},
        "spec": {
            "selector": {"matchLabels": {"app": "falco"}},
            "template": {
                "metadata": {"labels": {"app": "falco"}},
                "spec": {
                    "containers": [{
                        "name": "falco",
                        "image": "docker.io/falcosecurity/falco:0.40.0"
                    }]
                }
            }
        }
    }))
    .expect("daemonset");
    let found = match_daemon_set(&ds).expect("well-known falco name");
    assert_eq!(found.kind, WorkloadKind::Falco);
    assert_eq!(found.image, "docker.io/falcosecurity/falco:0.40.0");
}

#[test]
fn a_falcon_name_is_not_falco() {
    let svc: Service = serde_json::from_value(json!({
        "metadata": {"name": "falcon", "namespace": "default"}
    }))
    .expect("service");
    assert!(match_service(&svc).is_none());
}

#[test]
fn a_rules_configmap_keeps_keys_and_a_count_not_the_output_string() {
    let cm: ConfigMap = serde_json::from_value(json!({
        "metadata": {
            "name": "falco-rules",
            "namespace": "falco",
            "uid": "cm-1",
            "labels": {"falco-rules": "1"}
        },
        "data": {
            "rules.yaml": format!(
                "- rule: Planted\n  condition: always_true\n  output: leaked {PLANTED} and /etc/shadow\n  priority: WARNING\n"
            )
        }
    }))
    .expect("configmap");
    let map = match_rule_map(&cm).expect("falco-rules name");
    assert_eq!(map.keys, vec!["rules.yaml"]);
    assert_eq!(map.rule_count, 1);
    let debug = format!("{map:?}");
    assert!(
        !debug.contains(PLANTED),
        "rule output survived Debug: {debug}"
    );
    assert!(!debug.contains("/etc/shadow"), "{debug}");
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

impl TowerService<http::Request<Body>> for Script {
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
            match hit {
                Some(route) => {
                    route.used = true;
                    Some((route.status, route.body.clone()))
                }
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            }
        };
        Box::pin(async move {
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

fn empty_list(kind: &str) -> String {
    format!(r#"{{"kind":"{kind}List","apiVersion":"v1","metadata":{{}},"items":[]}}"#)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_every_falco_group_is_absent_and_does_not_list_crs() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.workloads, Workloads::Absent));
    assert!(matches!(inventory.rule_maps, RuleMaps::Absent));
    assert!(matches!(inventory.events, EventSet::NotServed));
    assert!(table_page(&inventory).is_none());
    assert!(
        script.seen().iter().all(|seen| {
            !seen.path.contains("/falcos?")
                && !seen.path.contains("/falcos/")
                && !seen.path.ends_with("/falcos")
                && !seen.path.contains("rulesfiles")
                && !seen.path.contains("falcorules")
                && !seen.path.contains("falcoevents")
        }),
        "a 404 group must not be chased into a kind list: {:?}",
        script.seen()
    );
    assert!(
        script.seen().iter().all(|seen| seen.method == "GET"),
        "a Falco fetch only reads: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_a_falco_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/falco.org", 403, STATUS_403);
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route("GET", "/api/v1/configmaps?", 200, empty_list("ConfigMap"));
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that group, not a whole-fetch failure");
    };
    let falco_org = inventory
        .groups
        .iter()
        .find(|(name, _)| name == "falco.org")
        .map(|(_, state)| state);
    assert_eq!(falco_org, Some(&GroupState::Denied));
    assert!(inventory.present(), "403 is visible, not served: false");
    assert!(table_page(&inventory).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_token_in_a_rules_configmap_does_not_appear_in_debug() {
    let script = Script::default();
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    // The sweep is metadata-only, so the list answers with what a real
    // server returns for that Accept header; the body arrives by get.
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        json!({
            "kind": "PartialObjectMetadataList",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": {},
            "items": [{
                "kind": "PartialObjectMetadata",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": {
                    "name": "falco-rules",
                    "namespace": "falco",
                    "uid": "cm-1",
                    "labels": {"falco-rules": "1"}
                }
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/falco/configmaps/falco-rules",
        200,
        json!({
            "kind": "ConfigMap",
            "apiVersion": "v1",
            "metadata": {
                "name": "falco-rules",
                "namespace": "falco",
                "uid": "cm-1",
                "labels": {"falco-rules": "1"}
            },
            "data": {
                "rules.yaml": format!(
                    "- rule: Planted\n  condition: evt.num>0\n  output: {PLANTED} /etc/shadow %proc.cmdline\n  priority: WARNING\n"
                )
            }
        })
        .to_string(),
    );
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a rules ConfigMap is inventory, not failure");
    };
    let maps = inventory.rule_maps.items();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].rule_count, 1);
    assert_eq!(maps[0].keys, vec!["rules.yaml"]);
    let debug = format!("{inventory:?}");
    let document = render(&inventory).join("\n");
    assert!(
        !debug.contains(PLANTED),
        "planted token survived Debug: {debug}"
    );
    assert!(
        !document.contains(PLANTED),
        "planted token survived render: {document}"
    );
    assert!(table_page(&inventory).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_crs_are_capped_and_a_served_falco_cr_is_listed() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/falco.org",
        200,
        r#"{"kind":"APIGroup","name":"falco.org",
            "versions":[{"groupVersion":"falco.org/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"falco.org/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/falco.org/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"falco.org/v1alpha1","resources":[
            {"name":"falcos","kind":"Falco","namespaced":true,"verbs":["get","list"]},
            {"name":"falcoevents","kind":"FalcoEvent","namespaced":true,"verbs":["get","list"]}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/falco.org/v1alpha1/falcos?",
        200,
        json!({
            "kind": "FalcoList",
            "items": [{
                "kind": "Falco",
                "metadata": {"name": "falco", "namespace": "falco", "uid": "cr-1"},
                "spec": {
                    "podTemplateSpec": {
                        "spec": {
                            "containers": [{
                                "name": "falco",
                                "image": "falcosecurity/falco:0.40.0"
                            }]
                        }
                    }
                },
                "status": {"conditions": [{"type": "Ready", "status": "True"}]}
            }]
        })
        .to_string(),
    );
    let mut event_items = Vec::new();
    for i in 0..(MAX_EVENTS + 8) {
        event_items.push(json!({
            "kind": "FalcoEvent",
            "metadata": {"name": format!("e{i}"), "namespace": "default"},
            "spec": {
                "rule": format!("r{i}"),
                "priority": "Warning",
                "time": "2024-01-01T00:00:00Z",
                "output": format!("cmdline {PLANTED}"),
                "output_fields": {
                    "k8s.ns.name": "default",
                    "k8s.pod.name": "p",
                    "proc.cmdline": PLANTED
                }
            }
        }));
    }
    script.route(
        "GET",
        "/apis/falco.org/v1alpha1/falcoevents?",
        200,
        json!({"kind": "FalcoEventList", "items": event_items}).to_string(),
    );
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route("GET", "/api/v1/configmaps?", 200, empty_list("ConfigMap"));

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    assert_eq!(inventory.resources.len(), 1);
    assert_eq!(inventory.resources[0].name, "falco");
    assert_eq!(inventory.resources[0].image, "falcosecurity/falco:0.40.0");
    // The falco.org group is the legacy chart shape, so `Ready` is the
    // condition this CR carries.
    assert_eq!(inventory.resources[0].ready, "Ready=True");
    let EventSet::Served { items, truncated } = &inventory.events else {
        panic!("event CRs are Served: {:?}", inventory.events);
    };
    assert_eq!(items.len(), MAX_EVENTS);
    assert!(*truncated);
    assert!(inventory.truncated);
    let debug = format!("{inventory:?}");
    assert!(
        !debug.contains(PLANTED),
        "event output survived Debug: {debug}"
    );
    assert!(table_page(&inventory).is_some());
}
