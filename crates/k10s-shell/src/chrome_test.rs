//! What the furniture says, checked without a window: the title bar names the
//! cluster rather than the fact of one, the status line always says which
//! cluster first and joins what is true in a fixed order, and a dragged dock
//! edge stops at its bounds.

use super::*;

#[test]
fn the_title_bar_names_the_cluster_rather_than_the_fact_of_one() {
    assert_eq!(
        Workspace::connection_label(true, Some("prod-eu-west")).as_ref(),
        "prod-eu-west"
    );
    assert_eq!(
        Workspace::connection_label(true, None).as_ref(),
        "in-cluster",
        "a service account has no context name and needs no invented one"
    );
    assert_eq!(
        Workspace::connection_label(false, Some("prod-eu-west")).as_ref(),
        "local starmap",
        "a context that is only remembered is not a connection"
    );
    assert_eq!(
        Workspace::connection_label(false, None).as_ref(),
        "local starmap"
    );
}

fn nothing() -> Status<'static> {
    Status {
        connected: false,
        context: None,
        selection: None,
        folder: None,
        panels_below: 0,
        note: None,
    }
}

#[test]
fn the_status_line_always_says_which_cluster_first() {
    assert_eq!(status_line(nothing()), "no cluster");
    assert_eq!(
        status_line(Status {
            connected: true,
            context: Some("prod-eu-west"),
            ..nothing()
        }),
        "connected to prod-eu-west"
    );
    assert_eq!(
        status_line(Status {
            connected: true,
            ..nothing()
        }),
        "connected in-cluster",
        "a service account has no context name to print"
    );
    assert_eq!(
        status_line(Status {
            connected: false,
            context: Some("prod-eu-west"),
            ..nothing()
        }),
        "no cluster",
        "a context that is only remembered is not a connection"
    );
}

#[test]
fn the_status_line_joins_what_is_true_in_a_fixed_order() {
    let line = status_line(Status {
        connected: true,
        context: Some("staging"),
        selection: Some(("pod", "api-7d9f")),
        folder: Some(std::path::Path::new("/home/k/charts")),
        panels_below: 2,
        note: Some("nothing chosen"),
    });
    assert_eq!(
        line,
        "connected to staging  ·  pod api-7d9f  ·  folder /home/k/charts  ·  \
             2 panels below  ·  nothing chosen"
    );
}

#[test]
fn one_panel_below_is_not_pluralised_and_none_is_not_mentioned() {
    let one = status_line(Status {
        panels_below: 1,
        ..nothing()
    });
    assert_eq!(one, "no cluster  ·  1 panel below");
    assert_eq!(
        status_line(Status {
            panels_below: 0,
            ..nothing()
        }),
        "no cluster",
        "an empty dock is not worth a clause"
    );
}

fn full_sample() -> UsageSample {
    UsageSample {
        cpu: Some(Millicores(250)),
        memory: Some(Bytes(32 * 1024 * 1024)),
        cpu_request: Some(Millicores(500)),
        cpu_limit: Some(Millicores(1000)),
        memory_request: Some(Bytes(64 * 1024 * 1024)),
        memory_limit: Some(Bytes(128 * 1024 * 1024)),
        source: UsageSource::MetricsServer,
        pods_measured: 1,
        pods_total: 1,
        truncated: false,
    }
}

#[test]
fn usage_lines_compute_percentages_at_display_time_from_the_typed_values() {
    assert_eq!(
        usage_lines(&full_sample()),
        [
            "CPU 250m · request 500m (50%) · limit 1 core (25%)",
            "Memory 32Mi · request 64Mi (50%) · limit 128Mi (25%)",
        ],
        "a single fully-measured pod needs no coverage or provenance clause"
    );
}

#[test]
fn an_unmeasured_value_says_sampling_and_never_renders_as_zero() {
    let first_kubelet_tick = UsageSample {
        cpu: None,
        memory: Some(Bytes(32 * 1024 * 1024)),
        cpu_limit: None,
        source: UsageSource::Kubelet,
        ..full_sample()
    };
    assert_eq!(
        usage_lines(&first_kubelet_tick),
        [
            "CPU sampling… · request 500m · no limit",
            "Memory 32Mi · request 64Mi (50%) · limit 128Mi (25%)",
            "via the kubelet; metrics-server is not installed",
        ],
        "no rate yet means no percentage against the request either"
    );
}

#[test]
fn what_was_never_declared_is_absent_from_the_sentence_not_zero_in_it() {
    let undeclared = UsageSample {
        cpu_request: None,
        cpu_limit: None,
        memory_request: None,
        memory_limit: None,
        ..full_sample()
    };
    assert_eq!(
        usage_lines(&undeclared),
        ["CPU 250m · no limit", "Memory 32Mi · no limit"],
        "a missing request contributes nothing; a missing limit is a fact worth a word"
    );
}

#[test]
fn a_workload_says_how_much_of_it_the_numbers_cover() {
    let partial = UsageSample {
        pods_measured: 2,
        pods_total: 3,
        ..full_sample()
    };
    assert!(
        usage_lines(&partial).contains(&"2 of 3 pods measured".to_string()),
        "a partial sum must say how partial it is: {:?}",
        usage_lines(&partial)
    );

    let clamped = UsageSample {
        pods_measured: 16,
        pods_total: 16,
        truncated: true,
        ..full_sample()
    };
    assert!(
        usage_lines(&clamped)
            .contains(&"16 of 16 pods measured; more match than are polled".to_string()),
        "a clamped workload names the clamp: {:?}",
        usage_lines(&clamped)
    );

    let unscraped_pod = UsageSample {
        cpu: None,
        memory: None,
        pods_measured: 0,
        ..full_sample()
    };
    assert!(
        usage_lines(&unscraped_pod).contains(&"0 of 1 pods measured".to_string()),
        "even one pod earns the clause when its numbers are missing: {:?}",
        usage_lines(&unscraped_pod)
    );
}

#[test]
fn the_shell_renders_units_the_same_way_the_data_plane_does() {
    // The seam mirrors the newtypes, so the rendering is pinned on both
    // sides; if either drifts, one of the two suites says so.
    assert_eq!(Millicores(250).to_string(), "250m");
    assert_eq!(Millicores(1000).to_string(), "1 core");
    assert_eq!(Millicores(1250).to_string(), "1.25 cores");
    assert_eq!(Millicores(2000).to_string(), "2 cores");
    assert_eq!(Bytes(512).to_string(), "512");
    assert_eq!(Bytes(800 * 1024).to_string(), "800Ki");
    assert_eq!(Bytes(512 * 1024 * 1024).to_string(), "512Mi");
    assert_eq!(Bytes(16 * 1024 * 1024 * 1024).to_string(), "16.0Gi");
    // The rungs themselves, exactly: a boundary byte climbs to the next unit.
    assert_eq!(Bytes(1024).to_string(), "1Ki");
    assert_eq!(Bytes(1024 * 1024).to_string(), "1Mi");
    assert_eq!(Bytes(1024 * 1024 * 1024).to_string(), "1.0Gi");
}

#[test]
fn a_dragged_dock_edge_stops_at_its_bounds() {
    let viewport = ui::Viewport {
        width: 1600.0,
        height: 1000.0,
    };
    let start = DockSizes {
        left: 300.0,
        right: 300.0,
        bottom: 200.0,
    };

    let left = dragged_dock_sizes(start, DockEdge::Left, 420.0, 0.0, viewport);
    assert_eq!(left.left, 420.0);
    assert_eq!(
        (left.right, left.bottom),
        (start.right, start.bottom),
        "dragging one edge moves one edge"
    );

    // The right dock is measured inward from the right of the window.
    let right = dragged_dock_sizes(start, DockEdge::Right, 1200.0, 0.0, viewport);
    assert_eq!(right.right, 400.0);

    // And the bottom upward from the top of the status bar.
    let bottom = dragged_dock_sizes(start, DockEdge::Bottom, 0.0, 700.0, viewport);
    assert_eq!(bottom.bottom, 1000.0 - STATUS_BAR_HEIGHT - 700.0);

    for (edge, x, y) in [
        (DockEdge::Left, -5000.0, 0.0),
        (DockEdge::Right, 9000.0, 0.0),
        (DockEdge::Bottom, 0.0, 9000.0),
    ] {
        let squashed = dragged_dock_sizes(start, edge, x, y, viewport);
        let measured = match edge {
            DockEdge::Left => squashed.left,
            DockEdge::Right => squashed.right,
            DockEdge::Bottom => squashed.bottom,
        };
        assert_eq!(
            measured, MIN_DOCK_SIZE,
            "a dock dragged shut stops at a size that can still be grabbed"
        );
    }
    for (edge, x, y) in [
        (DockEdge::Left, 9000.0, 0.0),
        (DockEdge::Right, -9000.0, 0.0),
        (DockEdge::Bottom, 0.0, -9000.0),
    ] {
        let stretched = dragged_dock_sizes(start, edge, x, y, viewport);
        let measured = match edge {
            DockEdge::Left => stretched.left,
            DockEdge::Right => stretched.right,
            DockEdge::Bottom => stretched.bottom,
        };
        assert_eq!(measured, MAX_DOCK_SIZE, "a dock cannot eat the window");
    }
}
