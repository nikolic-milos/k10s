//! Tetragon CRs listed through kube Request. Both group names are probed.
//! A planted kprobe arg filter must not appear in Debug.

use crate::*;
use k10s_data::read::Fetched;
use k10s_data::tetragon::{
    self, CILIUM_GROUP, GROUPS, GroupState, KindSet, TETRAGON_GROUP, WorkloadKind, WorkloadSet,
};

const PLANTED: &str = "planted-kprobe-arg-secret-7f3a";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn group_doc(group: &str, version: &str) -> String {
    format!(
        r#"{{"kind":"APIGroup","name":"{group}","versions":[{{"groupVersion":"{group}/{version}","version":"{version}"}}],"preferredVersion":{{"groupVersion":"{group}/{version}","version":"{version}"}}}}"#
    )
}

fn policy_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "sys-write", "uid": "tp-1" },
        "spec": {
            "kprobes": [{
                "call": "sys_write",
                "selectors": [{
                    "matchArgs": [{ "index": 1, "operator": "Equal", "values": [PLANTED] }]
                }]
            }],
            "lsmhooks": [{ "hook": "file_open" }],
            "podSelector": { "matchLabels": { "app": "xwing" } }
        },
        "status": { "state": "enabled" }
    })
}

fn list(items: &[serde_json::Value]) -> String {
    serde_json::json!({ "kind": "List", "metadata": {}, "items": items }).to_string()
}

#[test]
fn a_404_on_both_groups_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { tetragon::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.cilium, GroupState::NotServed));
    assert!(matches!(inventory.tetragon, GroupState::NotServed));
    assert!(tetragon::table_page(&inventory).is_none());
    let seen = script.seen();
    assert!(
        seen.iter().any(|s| s.path == "/apis/cilium.io"),
        "must probe {}: {seen:?}",
        GROUPS[0]
    );
    assert!(
        seen.iter().any(|s| s.path == "/apis/tetragon.io"),
        "must probe {}: {seen:?}",
        GROUPS[1]
    );
    assert!(
        script.requests_for("tracingpolicies").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_on_tetragon_io_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/tetragon.io", 403, status(403, "Forbidden"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { tetragon::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied: {fetched:?}");
    };
    assert!(matches!(inventory.tetragon, GroupState::Denied));
    assert!(inventory.served());
    assert!(matches!(inventory.tracing_policies, KindSet::Denied));
    assert!(tetragon::table_page(&inventory).is_some());
    drop(runtime);
}

#[test]
fn both_group_names_can_serve_tracingpolicies() {
    let runtime = runtime();

    let tetragon_script = Script::default();
    tetragon_script.route(
        "GET",
        "/apis/tetragon.io",
        200,
        group_doc(TETRAGON_GROUP, "v1alpha1"),
    );
    tetragon_script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpolicies?",
        200,
        list(&[policy_item()]),
    );
    tetragon_script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpoliciesnamespaced?",
        200,
        list(&[]),
    );
    tetragon_script.route("GET", "/apis/tetragon.io/v1alpha1/podinfo?", 200, list(&[]));

    let cilium_script = Script::default();
    cilium_script.route(
        "GET",
        "/apis/cilium.io",
        200,
        group_doc(CILIUM_GROUP, "v1alpha1"),
    );
    cilium_script.route(
        "GET",
        "/apis/cilium.io/v1alpha1/tracingpolicies?",
        200,
        list(&[policy_item()]),
    );
    cilium_script.route(
        "GET",
        "/apis/cilium.io/v1alpha1/tracingpoliciesnamespaced?",
        200,
        list(&[]),
    );
    cilium_script.route("GET", "/apis/cilium.io/v1alpha1/podinfo?", 200, list(&[]));

    let (from_tetragon, from_cilium) = runtime.block_on(async {
        (
            tetragon::fetch(&tetragon_script.client(), None).await,
            tetragon::fetch(&cilium_script.client(), None).await,
        )
    });
    let Fetched::Ok(from_tetragon) = from_tetragon else {
        panic!("tetragon.io listing must resolve");
    };
    let Fetched::Ok(from_cilium) = from_cilium else {
        panic!("cilium.io listing must resolve");
    };
    assert_eq!(
        from_tetragon.tracing_policies.items()[0].group,
        TETRAGON_GROUP
    );
    assert_eq!(from_cilium.tracing_policies.items()[0].group, CILIUM_GROUP);
    assert_eq!(from_tetragon.tracing_policies.items()[0].kprobes, 1);
    assert_eq!(from_tetragon.tracing_policies.items()[0].lsm, 1);
    assert!(tetragon::table_page(&from_tetragon).is_some());
    assert!(tetragon::table_page(&from_cilium).is_some());
    let debug = format!("{from_tetragon:?} {from_cilium:?}");
    assert!(
        !debug.contains(PLANTED),
        "a kprobe arg filter must not leak into Debug: {debug}"
    );
    drop(runtime);
}

#[test]
fn a_statusless_container_scoped_policy_says_so_in_its_row() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/tetragon.io",
        200,
        group_doc(TETRAGON_GROUP, "v1alpha1"),
    );
    script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpolicies?",
        200,
        list(&[serde_json::json!({
            "metadata": { "name": "container-scoped", "uid": "tp-2" },
            "spec": {
                "kprobes": [{ "call": "sys_open" }],
                "containerSelector": { "matchLabels": { "app": "xwing" } }
            }
        })]),
    );
    script.route(
        "GET",
        "/apis/tetragon.io/v1alpha1/tracingpoliciesnamespaced?",
        200,
        list(&[]),
    );
    script.route("GET", "/apis/tetragon.io/v1alpha1/podinfo?", 200, list(&[]));
    let runtime = runtime();
    let fetched = runtime.block_on(async { tetragon::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let policy = &inventory.tracing_policies.items()[0];
    assert!(policy.enabled, "noStatus upstream cannot report disabled");
    assert_eq!(policy.status, tetragon::STATUS_ABSENT);
    assert_eq!(policy.scope_selector, "container app=xwing");
    let page = tetragon::table_page(&inventory).expect("a served policy is a table");
    let row = page
        .rows
        .iter()
        .find(|row| row.name == "container-scoped")
        .expect("the policy row");
    assert_eq!(row.cells[4], tetragon::STATUS_ABSENT);
    assert_eq!(row.cells[6], "container app=xwing");
    drop(runtime);
}

#[test]
fn cilium_io_without_tracingpolicy_kinds_is_not_a_table() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc(CILIUM_GROUP, "v2"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { tetragon::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("CNP-only cilium.io is Ok: {fetched:?}");
    };
    assert!(matches!(inventory.cilium, GroupState::Served));
    assert!(matches!(inventory.tracing_policies, KindSet::NotServed));
    assert!(
        tetragon::table_page(&inventory).is_none(),
        "cilium.io/v2 CNP is not Tetragon"
    );
    drop(runtime);
}

#[test]
fn event_parser_fixtures_and_unbound_grpc() {
    assert!(matches!(
        tetragon::event_source(),
        tetragon::EventSource::Unbound { .. }
    ));
    let exec = br#"{"process_exec":{"process":{"binary":"/usr/bin/curl","flags":"execve rootcwd","pod":{"namespace":"default","name":"xwing"},"arguments":"https://ebpf.io"}}}"#;
    let kprobe = br#"{"processKprobe":{"process":{"binary":"/usr/sbin/sshd","flags":"procFS","pod":{"name":"sshd","namespace":"kube-system"}},"args":[{"file_arg":{"path":"/etc/shadow"}}]}}"#;
    let exec = tetragon::parse_events(exec).expect("exec");
    let kprobe = tetragon::parse_events(kprobe).expect("kprobe");
    assert_eq!(exec.events[0].kind, tetragon::ObservedKind::ProcessExec);
    assert_eq!(exec.events[0].names, "default/xwing");
    assert_eq!(exec.events[0].binary, "/usr/bin/curl");
    assert_eq!(kprobe.events[0].kind, tetragon::ObservedKind::ProcessKprobe);
    assert_eq!(kprobe.events[0].binary, "/usr/sbin/sshd");
    assert!(kprobe.events[0].args.contains("/etc/shadow"));
}

#[test]
fn a_tetragon_service_matches_the_workload_fingerprint() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        list(&[serde_json::json!({
            "metadata": {
                "name": "cilium-tetragon",
                "namespace": "kube-system"
            }
        })]),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { tetragon::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("workload listing must resolve: {fetched:?}");
    };
    let WorkloadSet::Found(found) = inventory.workload else {
        panic!("the Service is the fingerprint: {:?}", inventory.workload);
    };
    assert_eq!(found[0].kind, WorkloadKind::Service);
    assert_eq!(found[0].name, "cilium-tetragon");
    drop(runtime);
}
