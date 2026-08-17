//! Falco inventory against the scripted API server: 404/403, real JSON
//! parser fixtures, a planted token in a rules ConfigMap, and caps.

use crate::*;
use k10s_data::falco::{
    self, CrKind, EventSet, GroupState, MAX_EVENTS, OUTPUTS_UNBOUND, Outputs, RuleMaps, Workloads,
    parse_log_chunk,
};
use k10s_data::read::Fetched;

const PLANTED: &str = "PLANTED_FALCO_RULE_TOKEN_9f3a";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn empty_list(kind: &str) -> String {
    format!(r#"{{"kind":"{kind}List","apiVersion":"v1","metadata":{{}},"items":[]}}"#)
}

fn script_empty_core(script: &Script) {
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route("GET", "/api/v1/configmaps?", 200, empty_list("ConfigMap"));
}

/// Published Falco JSON from the outputs documentation.
fn terminal_shell_json() -> &'static str {
    r#"{"hostname":"falco-xczjd","output":"13:44:05.478445995: Critical A shell was spawned in a container with an attached terminal (user=root user_loginuid=-1 k8s.ns=default k8s.pod=kubecon container=ee97d9c4186f shell=sh parent=runc cmdline=sh -c clear; (bash || ash || sh) terminal=34816 container_id=ee97d9c4186f image=docker.io/library/alpine)","priority":"Critical","rule":"Terminal shell in container","source":"syscall","tags":["container","mitre_execution","shell"],"time":"2023-05-25T13:44:05.478445995Z","output_fields":{"container.id":"ee97d9c4186f","container.image.repository":"docker.io/library/alpine","evt.time":1685022245478445995,"k8s.ns.name":"default","k8s.pod.name":"kubecon","proc.cmdline":"sh -c clear; (bash || ash || sh)","proc.name":"sh","proc.pname":"runc","proc.tty":34816,"user.loginuid":-1,"user.name":"root"}}"#
}

fn write_below_binary_json() -> &'static str {
    r#"{"hostname":"ip-10-0-0-76.us-west-2.compute.internal","output":"10:20:05.211321183: Warning File below a known binary directory opened for writing (user=root command=touch /bin/foo file=/bin/foo)","output_fields":{"evt.time":1507021205211321183,"fd.name":"/bin/foo","proc.cmdline":"touch /bin/foo","user.name":"root","k8s.ns.name":"kube-system","k8s.pod.name":"coredns-abc"},"priority":"Warning","rule":"Write below binary dir","source":"syscall","tags":["filesystem","mitre_persistence"],"time":"2017-10-03T10:20:05.211321183Z"}"#
}

#[test]
fn a_404_falco_group_is_invisible_and_does_not_list_crs() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.workloads, Workloads::Absent));
    assert!(matches!(inventory.rule_maps, RuleMaps::Absent));
    assert!(matches!(inventory.events, EventSet::NotServed));
    assert!(
        falco::table_page(&inventory).is_none(),
        "nothing served and no workload found is a hidden pane"
    );
    assert!(
        script.seen().iter().all(|seen| {
            !seen.path.contains("/falcos?")
                && !seen.path.contains("/falcos/")
                && !seen.path.ends_with("/falcos")
                && !seen.path.contains("rulesfiles")
                && !seen.path.contains("falcorules")
                && !seen.path.contains("falcoevents")
                && !seen.path.contains("falcotools")
                && !seen.path.contains("falcosidekicks")
        }),
        "a 404 group must not be chased into a kind list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_falco_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/falco.org", 403, status(403, "Forbidden"));
    script_empty_core(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that group: {fetched:?}");
    };
    let falco_org = inventory
        .groups
        .iter()
        .find(|(name, _)| name == "falco.org")
        .map(|(_, state)| state);
    assert_eq!(falco_org, Some(&GroupState::Denied));
    assert!(
        inventory.present(),
        "403 is Denied, not an invisible cluster"
    );
    let page = falco::table_page(&inventory).expect("a denied group is a labelled row");
    assert!(
        page.rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| cell.contains("access denied"))),
        "{page:?}"
    );
    drop(runtime);
}

#[test]
fn a_403_on_a_cr_list_under_a_served_group_is_a_denied_kind_not_zero_objects() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev",
        200,
        r#"{"kind":"APIGroup","name":"instance.falcosecurity.dev",
            "versions":[{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"instance.falcosecurity.dev/v1alpha1","resources":[
            {"name":"falcos","kind":"Falco","namespaced":true,"verbs":["get","list"]}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1/falcos?",
        403,
        status(403, "Forbidden"),
    );
    script_empty_core(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a denied collection is recorded, not a whole-fetch failure: {fetched:?}");
    };
    assert_eq!(
        inventory.denied_kinds,
        vec![(
            "instance.falcosecurity.dev".to_string(),
            "falcos".to_string()
        )]
    );
    assert!(inventory.resources.is_empty());
    let document = falco::render(&inventory).join("\n");
    assert!(
        !document.contains("no Falco objects are stored"),
        "a denied account was told the operator holds nothing: {document}"
    );
    assert!(
        document.contains("instance.falcosecurity.dev/falcos: access denied for this account"),
        "{document}"
    );
    let page = falco::table_page(&inventory).expect("a served group is visible");
    assert!(
        page.rows
            .iter()
            .any(|row| row.uid == "denied:instance.falcosecurity.dev/falcos"),
        "{page:?}"
    );
    drop(runtime);
}

#[test]
fn a_403_on_resource_discovery_is_recorded_and_not_chased_into_fallback_lists() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev",
        200,
        r#"{"kind":"APIGroup","name":"instance.falcosecurity.dev",
            "versions":[{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1",
        403,
        status(403, "Forbidden"),
    );
    script_empty_core(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("denied discovery is recorded, not a whole-fetch failure: {fetched:?}");
    };
    assert_eq!(
        inventory.denied_kinds,
        vec![("instance.falcosecurity.dev".to_string(), String::new())]
    );
    assert!(
        script.seen().iter().all(|seen| {
            !seen
                .path
                .contains("/apis/instance.falcosecurity.dev/v1alpha1/")
        }),
        "denied discovery must not be chased into fallback kind lists: {:?}",
        script.seen()
    );
    let document = falco::render(&inventory).join("\n");
    assert!(
        !document.contains("no Falco objects are stored"),
        "{document}"
    );
    assert!(
        document.contains("instance.falcosecurity.dev: resource discovery denied for this account"),
        "{document}"
    );
    drop(runtime);
}

#[test]
fn a_config_artifact_cr_is_listed_and_its_inline_fragment_stays_behind() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/artifact.falcosecurity.dev",
        200,
        r#"{"kind":"APIGroup","name":"artifact.falcosecurity.dev",
            "versions":[{"groupVersion":"artifact.falcosecurity.dev/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"artifact.falcosecurity.dev/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/artifact.falcosecurity.dev/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"artifact.falcosecurity.dev/v1alpha1","resources":[
            {"name":"configs","kind":"Config","namespaced":true,"verbs":["get","list"]},
            {"name":"rulesfiles","kind":"Rulesfile","namespaced":true,"verbs":["get","list"]}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/artifact.falcosecurity.dev/v1alpha1/configs?",
        200,
        serde_json::json!({
            "kind": "ConfigList",
            "items": [{
                "kind": "Config",
                "metadata": {"name": "sidekick-config", "namespace": "falco", "uid": "cfg-1"},
                "spec": {
                    "config": format!("falcosidekick:\n  webhook:\n    address: https://hook.example/{PLANTED}\n"),
                    "configMapRef": {"name": "sidekick-fragment"},
                    "priority": 10
                },
                "status": {"conditions": [{"type": "Programmed", "status": "True"}]}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/artifact.falcosecurity.dev/v1alpha1/rulesfiles?",
        200,
        empty_list("Rulesfile"),
    );
    script_empty_core(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served Config listing must resolve: {fetched:?}");
    };
    assert_eq!(inventory.resources.len(), 1);
    assert_eq!(inventory.resources[0].kind, CrKind::FalcoConfig);
    assert_eq!(inventory.resources[0].kind_name, "Config");
    assert_eq!(inventory.resources[0].name, "sidekick-config");
    assert_eq!(inventory.resources[0].ready, "Programmed=True");
    assert_eq!(inventory.resources[0].rules_refs, vec!["sidekick-fragment"]);
    let debug = format!("{inventory:?}");
    assert!(
        !debug.contains(PLANTED),
        "the inline config fragment survived Debug: {debug}"
    );
    drop(runtime);
}

#[test]
fn a_rule_map_sweep_reads_metadata_and_fetches_only_the_matching_body() {
    let script = Script::default();
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
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
            }, {
                "kind": "PartialObjectMetadata",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": {"name": "ca-bundle", "namespace": "kube-system", "uid": "cm-2"}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/falco/configmaps/falco-rules",
        200,
        serde_json::json!({
            "kind": "ConfigMap",
            "apiVersion": "v1",
            "metadata": {
                "name": "falco-rules",
                "namespace": "falco",
                "uid": "cm-1",
                "labels": {"falco-rules": "1"}
            },
            "data": {
                "rules.yaml": "- rule: One\n  condition: evt.num>0\n  priority: WARNING\n"
            }
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a matched rules ConfigMap is inventory: {fetched:?}");
    };
    let maps = inventory.rule_maps.items();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].name, "falco-rules");
    assert_eq!(maps[0].rule_count, 1);
    let sweep = script
        .requests_for("/api/v1/configmaps")
        .into_iter()
        .next()
        .expect("the sweep lists ConfigMaps");
    assert!(
        sweep.accept.contains("PartialObjectMetadataList"),
        "the sweep must not pull ConfigMap bodies: accept was {}",
        sweep.accept
    );
    assert!(
        script
            .requests_for("/api/v1/namespaces/kube-system/configmaps/ca-bundle")
            .is_empty(),
        "an unmatched ConfigMap's body was fetched: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_namespace_scoped_fetch_lists_that_namespace_and_keeps_cluster_scoped_kinds_at_the_cluster_path()
 {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev",
        200,
        r#"{"kind":"APIGroup","name":"instance.falcosecurity.dev",
            "versions":[{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}}"#,
    );
    // Components is scripted as cluster-scoped so the discovery flag, not
    // the requested namespace alone, is what picks the collection path.
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"instance.falcosecurity.dev/v1alpha1","resources":[
            {"name":"falcos","kind":"Falco","namespaced":true,"verbs":["get","list"]},
            {"name":"components","kind":"Component","namespaced":false,"verbs":["get","list"]}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1/namespaces/falco/falcos?",
        200,
        serde_json::json!({
            "kind": "FalcoList",
            "items": [{
                "kind": "Falco",
                "metadata": {"name": "falco", "namespace": "falco", "uid": "cr-1"},
                "spec": {"version": "0.40.0"},
                "status": {"conditions": [{"type": "Available", "status": "True"}]}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1/components?",
        200,
        empty_list("Component"),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/falco/services?",
        200,
        empty_list("Service"),
    );
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/falco/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/falco/configmaps?",
        200,
        serde_json::json!({
            "kind": "PartialObjectMetadataList",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": {},
            "items": [{
                "kind": "PartialObjectMetadata",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": {"name": "falco-rules", "namespace": "falco", "uid": "cm-1"}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/falco/configmaps/falco-rules",
        200,
        serde_json::json!({
            "kind": "ConfigMap",
            "apiVersion": "v1",
            "metadata": {"name": "falco-rules", "namespace": "falco", "uid": "cm-1"},
            "data": {"rules.yaml": "- rule: One\n  condition: evt.num>0\n"}
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), Some("falco")).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a namespace-scoped fetch must resolve: {fetched:?}");
    };
    assert_eq!(inventory.resources.len(), 1);
    assert_eq!(inventory.resources[0].namespace, "falco");
    assert_eq!(inventory.rule_maps.items().len(), 1);
    let seen = script.seen();
    assert!(
        seen.iter().any(|seen| seen
            .path
            .contains("/apis/instance.falcosecurity.dev/v1alpha1/namespaces/falco/falcos?")),
        "a namespaced kind must be listed under the namespace: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|seen| !seen.path.contains("/v1alpha1/falcos?")),
        "a scoped fetch must not also sweep the cluster collection: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|seen| seen.path.contains("/v1alpha1/components?")),
        "a cluster-scoped kind stays at the cluster collection: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|seen| !seen.path.contains("/namespaces/falco/components")),
        "the discovery flag, not the namespace, picks the path: {seen:?}"
    );
    for scoped in [
        "/api/v1/namespaces/falco/services?",
        "/apis/apps/v1/namespaces/falco/daemonsets?",
        "/api/v1/namespaces/falco/configmaps?",
    ] {
        assert!(
            seen.iter().any(|seen| seen.path.contains(scoped)),
            "the core scans must honour the namespace: {seen:?}"
        );
    }
    for cluster_wide in [
        "/api/v1/services?",
        "/apis/apps/v1/daemonsets?",
        "/api/v1/configmaps?",
    ] {
        assert!(
            seen.iter().all(|seen| !seen.path.starts_with(cluster_wide)),
            "a scoped fetch must not scan the whole cluster: {seen:?}"
        );
    }
    drop(runtime);
}

#[test]
fn parser_fixtures_from_real_falco_json_keep_priority_rule_and_pod() {
    let events = parse_log_chunk(terminal_shell_json());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].priority, "Critical");
    assert_eq!(events[0].rule, "Terminal shell in container");
    assert_eq!(events[0].namespace, "default");
    assert_eq!(events[0].pod, "kubecon");
    assert_eq!(events[0].time, "2023-05-25T13:44:05.478445995Z");

    let events = parse_log_chunk(write_below_binary_json());
    assert_eq!(events[0].priority, "Warning");
    assert_eq!(events[0].rule, "Write below binary dir");
    assert_eq!(events[0].namespace, "kube-system");
    assert_eq!(events[0].pod, "coredns-abc");
    let debug = format!("{:?}", events[0]);
    assert!(!debug.contains("/bin/foo"), "{debug}");
    assert!(!debug.contains("touch"), "{debug}");

    let prefixed = format!("2023-05-25T13:44:05.478445995Z {}", terminal_shell_json());
    assert_eq!(
        parse_log_chunk(&prefixed)[0].rule,
        "Terminal shell in container"
    );
}

#[test]
fn a_planted_token_in_a_rule_configmap_does_not_leak() {
    let script = Script::default();
    script.route("GET", "/api/v1/services?", 200, empty_list("Service"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    // The sweep asks for metadata only; the matched map's body arrives by
    // a get of that one object.
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
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
        serde_json::json!({
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
                    "- rule: Planted\n  condition: evt.num>0\n  output: {PLANTED} /etc/shadow %proc.cmdline\n  priority: WARNING\n- rule: Second\n  condition: never_true\n  output: also {PLANTED}\n  priority: ERROR\n"
                )
            }
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a rules ConfigMap is inventory: {fetched:?}");
    };
    let maps = inventory.rule_maps.items();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].name, "falco-rules");
    assert_eq!(maps[0].keys, vec!["rules.yaml"]);
    assert_eq!(maps[0].rule_count, 2);
    let debug = format!("{inventory:?}");
    let document = falco::render(&inventory).join("\n");
    assert!(
        !debug.contains(PLANTED),
        "planted token survived Debug: {debug}"
    );
    assert!(
        !document.contains(PLANTED),
        "planted token survived render: {document}"
    );
    assert!(!debug.contains("/etc/shadow"), "{debug}");
    drop(runtime);
}

#[test]
fn event_crs_and_log_chunks_are_capped_and_outputs_stay_unbound() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev",
        200,
        r#"{"kind":"APIGroup","name":"instance.falcosecurity.dev",
            "versions":[{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"instance.falcosecurity.dev/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"instance.falcosecurity.dev/v1alpha1","resources":[
            {"name":"falcos","kind":"Falco","namespaced":true,"verbs":["get","list"]}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/instance.falcosecurity.dev/v1alpha1/falcos?",
        200,
        serde_json::json!({
            "kind": "FalcoList",
            "items": [{
                "kind": "Falco",
                "metadata": {"name": "falco", "namespace": "falco", "uid": "cr-1"},
                "spec": {"version": "0.40.0", "configMapRef": {"name": "extra-rules"}},
                "status": {"conditions": [
                    {"type": "Reconciled", "status": "True"},
                    {"type": "Available", "status": "True"}
                ]}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/events.falco.org",
        200,
        r#"{"kind":"APIGroup","name":"events.falco.org",
            "versions":[{"groupVersion":"events.falco.org/v1alpha1","version":"v1alpha1"}],
            "preferredVersion":{"groupVersion":"events.falco.org/v1alpha1","version":"v1alpha1"}}"#,
    );
    script.route(
        "GET",
        "/apis/events.falco.org/v1alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"events.falco.org/v1alpha1","resources":[
            {"name":"falcoevents","kind":"FalcoEvent","namespaced":true,"verbs":["get","list"]}
        ]}"#,
    );
    let mut event_items = Vec::new();
    for i in 0..(MAX_EVENTS + 12) {
        event_items.push(serde_json::json!({
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
        "/apis/events.falco.org/v1alpha1/falcoevents?",
        200,
        serde_json::json!({"kind": "FalcoEventList", "items": event_items}).to_string(),
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        serde_json::json!({
            "kind": "ServiceList",
            "items": [{
                "metadata": {
                    "name": "falco",
                    "namespace": "falco",
                    "uid": "svc-1",
                    "labels": {"app.kubernetes.io/name": "falco"}
                },
                "spec": {"ports": [{"name": "outputs-grpc", "port": 5060}]}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        200,
        empty_list("DaemonSet"),
    );
    script.route("GET", "/api/v1/configmaps?", 200, empty_list("ConfigMap"));

    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert_eq!(inventory.resources.len(), 1);
    assert_eq!(inventory.resources[0].kind, CrKind::Falco);
    assert_eq!(inventory.resources[0].image, "0.40.0");
    assert_eq!(inventory.resources[0].ready, "Available=True");
    assert_eq!(inventory.resources[0].rules_refs, vec!["extra-rules"]);
    assert!(inventory.workloads.found());
    let EventSet::Served { items, truncated } = &inventory.events else {
        panic!("event CRs are Served: {:?}", inventory.events);
    };
    assert_eq!(items.len(), MAX_EVENTS);
    assert!(*truncated);
    assert!(inventory.truncated);
    assert_eq!(
        inventory.outputs,
        Outputs::Unbound {
            why: OUTPUTS_UNBOUND.to_string()
        }
    );
    let debug = format!("{inventory:?}");
    assert!(
        !debug.contains(PLANTED),
        "event output survived Debug: {debug}"
    );
    let page = falco::table_page(&inventory).expect("Falco is present");
    assert!(
        page.rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| cell == "Unbound")),
        "gRPC outputs stay Unbound: {page:?}"
    );

    let mut chunk = String::new();
    for i in 0..(MAX_EVENTS + 5) {
        chunk.push_str(&format!(
            r#"{{"rule":"log{i}","priority":"Warning","time":"t","output_fields":{{"k8s.ns.name":"ns","k8s.pod.name":"p"}}}}"#
        ));
        chunk.push('\n');
    }
    assert_eq!(parse_log_chunk(&chunk).len(), MAX_EVENTS);
    drop(runtime);
}

#[test]
fn a_403_on_services_without_a_served_group_stays_hidden() {
    let script = Script::default();
    script.route("GET", "/api/v1/services?", 403, status(403, "Forbidden"));
    script.route(
        "GET",
        "/apis/apps/v1/daemonsets?",
        403,
        status(403, "Forbidden"),
    );
    script.route("GET", "/api/v1/configmaps?", 200, empty_list("ConfigMap"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { falco::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a core 403 is Denied on workloads, not a fetch failure: {fetched:?}");
    };
    assert!(matches!(inventory.workloads, Workloads::Denied));
    assert!(
        !inventory.present(),
        "a Services 403 is not a Falco group and not a found workload"
    );
    assert!(falco::table_page(&inventory).is_none());
    drop(runtime);
}
