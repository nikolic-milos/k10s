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
