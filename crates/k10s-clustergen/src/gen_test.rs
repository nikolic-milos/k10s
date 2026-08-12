use k10s_core::{KindId, ToolId};

use crate::*;

fn cfg(seed: u64, target_objects: u32) -> GenConfig {
    GenConfig {
        seed,
        target_objects,
        scenario: Scenario::Platform,
    }
}

#[test]
fn deterministic() {
    let a = generate(&cfg(42, 5000));
    let b = generate(&cfg(42, 5000));
    assert_eq!(a.namespaces.len(), b.namespaces.len());
    assert_eq!(a.total_pods, b.total_pods);
    assert_eq!(a.total_sats, b.total_sats);
    assert_eq!(
        a.namespaces[0].workloads[0].name,
        b.namespaces[0].workloads[0].name
    );
    let sat_a = a
        .namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .find_map(|w| w.sats.first());
    let sat_b = b
        .namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .find_map(|w| w.sats.first());
    assert_eq!(sat_a.map(|s| &s.name), sat_b.map(|s| &s.name));
}

#[test]
fn hits_target_roughly() {
    let spec = generate(&cfg(7, 50_000));
    let total =
        spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
    assert!((50_000..50_600).contains(&total), "total = {total}");
}

#[test]
fn satellite_mix_is_consistent() {
    let spec = generate(&cfg(42, 50_000));
    assert!(spec.total_sats > 0);
    let mut pvc = 0u32;
    let mut sts_pods = 0u32;
    for wl in spec.namespaces.iter().flat_map(|n| &n.workloads) {
        if wl.kind == KindId::STATEFUL_SET {
            sts_pods += wl.pods.len() as u32;
            let vols = wl.sats.iter().filter(|s| s.kind == KindId::VOLUME).count() as u32;
            assert_eq!(vols, wl.pods.len() as u32, "one PVC per sts replica");
            pvc += vols;
        } else {
            assert!(
                wl.sats.iter().all(|s| s.kind != KindId::VOLUME),
                "only StatefulSets own PVCs"
            );
        }
    }
    assert_eq!(pvc, sts_pods);
    for sat in spec
        .namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .flat_map(|w| &w.sats)
    {
        assert!(!sat.name.is_empty() && !sat.detail.is_empty());
    }
}

#[test]
fn scenarios_shape_the_cluster() {
    let platform = generate(&cfg(42, 20_000));
    assert!(
        platform
            .namespaces
            .iter()
            .any(|n| n.name == "observability")
    );
    assert!(platform.namespaces.iter().any(|n| n.name == "databases"));

    let obs = generate(&GenConfig {
        seed: 42,
        target_objects: 20_000,
        scenario: Scenario::Observability,
    });
    assert!(obs.namespaces.iter().any(|n| n.name == "observability"));
    assert!(!obs.namespaces.iter().any(|n| n.name == "streaming"));

    let prom = obs
        .namespaces
        .iter()
        .find(|n| n.name == "observability")
        .and_then(|n| n.workloads.iter().find(|w| w.name == "prometheus-server"))
        .expect("observability runs prometheus-server");
    assert_eq!(prom.tool, ToolId::PROMETHEUS);
    assert_eq!(prom.kind, KindId::STATEFUL_SET);
    let obs2 = generate(&GenConfig {
        seed: 42,
        target_objects: 20_000,
        scenario: Scenario::Observability,
    });
    assert_eq!(obs.total_pods, obs2.total_pods);
}

fn fan_cfg(scenario: Scenario, target_objects: u32) -> GenConfig {
    GenConfig {
        seed: 42,
        target_objects,
        scenario,
    }
}

fn widest_ns(spec: &ClusterSpec) -> &NsSpec {
    spec.namespaces
        .iter()
        .max_by_key(|n| n.workloads.len())
        .expect("some namespace")
}

fn deepest_wl(spec: &ClusterSpec) -> &WorkloadSpec {
    spec.namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .max_by_key(|w| w.pods.len())
        .expect("some workload")
}

#[test]
fn ns_fan_out_concentrates_workloads_in_one_namespace() {
    let spec = generate(&fan_cfg(Scenario::NsFanOut, 25_000));
    let hot = widest_ns(&spec);
    assert_eq!(hot.name, "monorepo-prod");
    assert!(
        hot.workloads.len() >= 4_000,
        "fan-out degree = {}",
        hot.workloads.len()
    );

    let rest: usize = spec
        .namespaces
        .iter()
        .filter(|n| n.name != hot.name)
        .map(|n| n.workloads.len())
        .max()
        .unwrap_or(0);
    assert!(
        hot.workloads.len() > rest * 20,
        "hot {} vs widest other {rest}",
        hot.workloads.len()
    );
    assert!(
        spec.namespaces.len() <= 16,
        "few namespaces, got {}",
        spec.namespaces.len()
    );
    assert!(
        hot.workloads.iter().all(|w| w.pods.len() <= 3),
        "the budget must buy workload count, not pods"
    );

    let total =
        spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
    assert!((25_000..25_600).contains(&total), "total = {total}");
}

#[test]
fn wl_fan_out_concentrates_pods_on_one_workload() {
    let spec = generate(&fan_cfg(Scenario::WlFanOut, 25_000));
    let hot = deepest_wl(&spec);
    assert_eq!(hot.name, "shard-prod-shard");
    assert_eq!(hot.kind, KindId::STATEFUL_SET);
    assert!(
        hot.pods.len() >= 4_000,
        "fan-out degree = {}",
        hot.pods.len()
    );

    let vols = hot.sats.iter().filter(|s| s.kind == KindId::VOLUME).count();
    assert_eq!(vols, hot.pods.len(), "the sat ring fans out with the pods");

    let shard_ns = spec
        .namespaces
        .iter()
        .find(|n| n.name == "shard-prod")
        .expect("shard-prod exists");
    assert!(
        shard_ns.workloads.len() <= FAN_WL_SIBLINGS as usize + 1,
        "few workloads, got {}",
        shard_ns.workloads.len()
    );

    let second = spec
        .namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .filter(|w| w.name != hot.name)
        .map(|w| w.pods.len())
        .max()
        .unwrap_or(0);
    assert!(hot.pods.len() > second * 20, "hot vs second {second}");

    let total =
        spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
    assert!((25_000..25_600).contains(&total), "total = {total}");
}

#[test]
fn fan_out_degree_scales_with_the_object_budget() {
    let mut ns_degrees = Vec::new();
    let mut wl_degrees = Vec::new();
    for target in [2_000u32, 6_000, 25_000] {
        ns_degrees.push(
            widest_ns(&generate(&fan_cfg(Scenario::NsFanOut, target)))
                .workloads
                .len(),
        );
        wl_degrees.push(
            deepest_wl(&generate(&fan_cfg(Scenario::WlFanOut, target)))
                .pods
                .len(),
        );
    }
    assert!(
        ns_degrees.windows(2).all(|w| w[1] > w[0] * 2),
        "ns degrees {ns_degrees:?}"
    );
    assert!(
        wl_degrees.windows(2).all(|w| w[1] > w[0] * 2),
        "wl degrees {wl_degrees:?}"
    );
}

#[test]
fn fan_out_scenarios_are_deterministic() {
    for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
        let a = generate(&fan_cfg(scenario, 12_000));
        let b = generate(&fan_cfg(scenario, 12_000));
        assert_eq!(a.namespaces.len(), b.namespaces.len());
        assert_eq!(a.total_workloads, b.total_workloads);
        assert_eq!(a.total_pods, b.total_pods);
        assert_eq!(a.total_sats, b.total_sats);
        assert_eq!(a.total_edges, b.total_edges);
        assert_eq!(
            widest_ns(&a).workloads.len(),
            widest_ns(&b).workloads.len(),
            "{}",
            scenario.as_str()
        );
        assert_eq!(deepest_wl(&a).pods.len(), deepest_wl(&b).pods.len());
        for (x, y) in a.namespaces.iter().zip(&b.namespaces) {
            assert_eq!(x.name, y.name);
            assert_eq!(x.workloads.len(), y.workloads.len());
        }
        let names_a: Vec<&str> = widest_ns(&a).workloads.iter().map(|w| &*w.name).collect();
        let names_b: Vec<&str> = widest_ns(&b).workloads.iter().map(|w| &*w.name).collect();
        assert_eq!(names_a, names_b);
    }
}

#[test]
fn workload_names_are_unique_inside_every_namespace() {
    for scenario in [
        Scenario::Platform,
        Scenario::Observability,
        Scenario::Data,
        Scenario::NsFanOut,
        Scenario::WlFanOut,
    ] {
        let spec = generate(&GenConfig {
            seed: 55,
            target_objects: 25_000,
            scenario,
        });
        for ns in &spec.namespaces {
            let mut names: Vec<&str> = ns.workloads.iter().map(|w| &*w.name).collect();
            let count = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                count,
                "{:?}/{} holds two workloads of one name, which no cluster can",
                scenario,
                ns.name
            );
        }
    }
}

#[test]
fn fan_out_scenarios_round_trip_through_parse() {
    for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
        assert_eq!(Scenario::parse(scenario.as_str()), Some(scenario));
    }
    assert_eq!(Scenario::parse("ns-fanout"), Some(Scenario::NsFanOut));
    assert_eq!(Scenario::parse("wl-fanout"), Some(Scenario::WlFanOut));
    assert_eq!(Scenario::parse("fanout"), None);
}

#[test]
fn sts_pods_use_ordinals() {
    let spec = generate(&cfg(42, 20_000));
    let sts = spec
        .namespaces
        .iter()
        .flat_map(|n| &n.workloads)
        .find(|w| w.kind == KindId::STATEFUL_SET)
        .expect("some sts");
    assert!(sts.pods[0].name.ends_with("-0"), "{}", sts.pods[0].name);
}
