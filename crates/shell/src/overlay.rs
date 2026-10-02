//! Resolve overlay stamps onto the published scene, off the paint path.
//!
//! The data plane returns uid and namespace/name. This module joins those to
//! the snapshot the map is showing, then hands [`k10s_map::OverlayFrame`] to
//! `MapView::set_overlay`. First paint never waits here.

use std::collections::HashMap;

use k10s_core::SceneSnapshot;
use k10s_map::{OverlayFrame, OverlayKind, OverlayMark};
use k10s_theme::{Sample, Series, downsample_samples};

use crate::provider::OverlayStamp;

/// Join fetched stamps to the snapshot. A uid the scene does not carry, or a
/// name that matches nothing, is skipped: missing overlay, not a default colour.
pub fn resolve_frame(
    kind: OverlayKind,
    stamps: &[OverlayStamp],
    snapshot: &SceneSnapshot,
) -> OverlayFrame {
    let names = NameIndex::build(snapshot);
    let mut marks = Vec::with_capacity(stamps.len().min(512));
    for stamp in stamps {
        let Some(uid) = resolve_uid(stamp, snapshot, &names) else {
            continue;
        };
        marks.push(OverlayMark {
            uid,
            tint: stamp.tint,
            sparkline: sparkline_of(stamp),
            label: stamp.label.clone(),
        });
    }
    OverlayFrame {
        kind: Some(kind),
        marks,
    }
}

fn resolve_uid(
    stamp: &OverlayStamp,
    snapshot: &SceneSnapshot,
    names: &NameIndex,
) -> Option<String> {
    if !stamp.uid.is_empty() && snapshot.locate(&stamp.uid).is_some() {
        return Some(stamp.uid.clone());
    }
    names.get(&stamp.namespace, &stamp.name)
}

fn sparkline_of(stamp: &OverlayStamp) -> Option<Series> {
    if stamp.samples.len() < 2 {
        return None;
    }
    let samples: Vec<Sample> = stamp
        .samples
        .iter()
        .copied()
        .map(|(t_ms, value)| Sample { t_ms, value })
        .collect();
    let samples = downsample_samples(&samples, 32);
    if samples.len() < 2 {
        return None;
    }
    Some(Series {
        name: stamp.label.clone().unwrap_or_else(|| stamp.name.clone()),
        samples,
    })
}

struct NameIndex {
    by_ns_name: HashMap<(String, String), String>,
}

impl NameIndex {
    fn build(snapshot: &SceneSnapshot) -> NameIndex {
        let mut by_ns_name = HashMap::new();
        let ids = &snapshot.ids;
        for (slot, region) in snapshot.regions.iter().enumerate() {
            if let Some(uid) = ids.regions.get(slot).filter(|uid| !uid.is_empty()) {
                by_ns_name.insert((String::new(), region.label.to_string()), uid.to_string());
            }
            snapshot.for_each_region_block(slot, |block_slot, block| {
                if let Some(uid) = ids.blocks.get(block_slot).filter(|uid| !uid.is_empty()) {
                    by_ns_name.insert(
                        (region.label.to_string(), block.label.to_string()),
                        uid.to_string(),
                    );
                }
                snapshot.for_each_block_cell(block_slot, |cell, pod| {
                    if let Some(uid) = ids.cells.get(cell).filter(|uid| !uid.is_empty()) {
                        by_ns_name.insert(
                            (region.label.to_string(), pod.label.to_string()),
                            uid.to_string(),
                        );
                    }
                });
                snapshot.for_each_block_sat(block_slot, |sat, node| {
                    if let Some(uid) = ids.sats.get(sat).filter(|uid| !uid.is_empty()) {
                        by_ns_name.insert(
                            (region.label.to_string(), node.label.to_string()),
                            uid.to_string(),
                        );
                    }
                });
            });
        }
        NameIndex { by_ns_name }
    }

    fn get(&self, namespace: &str, name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        self.by_ns_name
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
    }
}

pub fn next_overlay(kind: Option<OverlayKind>) -> Option<OverlayKind> {
    match kind {
        None => Some(OverlayKind::Sync),
        Some(kind) => kind.next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{
        KindId, NsExt, NsNode, PodExt, PodNode, Rect, Severity, State, ToolId, WlExt, WorkloadNode,
    };
    use std::sync::Arc;

    fn snapshot() -> SceneSnapshot {
        let mut snap = SceneSnapshot::default();
        snap.identity_rev = 1;
        snap.rev = 1;
        let ids = Arc::make_mut(&mut snap.ids);
        snap.scene.cells.push(PodNode {
            rect: Rect::new(0.0, 0.0, 8.0, 8.0),
            label: Arc::from("api-0"),
            ext: PodExt { state: State::OK },
        });
        ids.cells.push(Arc::from("pod-uid"));
        snap.scene.blocks.push(WorkloadNode {
            rect: Rect::new(0.0, 0.0, 60.0, 40.0),
            inner: Rect::new(2.0, 2.0, 56.0, 36.0),
            label: Arc::from("api"),
            children: 0..1,
            sats: 0..0,
            ext: WlExt {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                rollup: Severity::Ok,
                ns: 0,
            },
        });
        ids.blocks.push(Arc::from("wl-api"));
        snap.scene.regions.push(NsNode {
            rect: Rect::new(0.0, 0.0, 400.0, 300.0),
            label: Arc::from("prod"),
            weight: 0,
            children: 0..1,
            ext: NsExt {
                unhealthy_frac: 0.0,
                rollup: Severity::Ok,
            },
        });
        ids.regions.push(Arc::from("ns-prod"));
        snap
    }

    #[test]
    fn a_stamp_joins_by_uid_or_by_namespace_and_name() {
        let snap = snapshot();
        let by_uid = OverlayStamp {
            uid: "pod-uid".into(),
            namespace: String::new(),
            name: String::new(),
            tint: Some(Severity::Warn),
            samples: Vec::new(),
            label: Some("isolated".into()),
        };
        let by_name = OverlayStamp {
            uid: String::new(),
            namespace: "prod".into(),
            name: "api-0".into(),
            tint: Some(Severity::Err),
            samples: vec![(1, 1.0), (2, 2.0)],
            label: None,
        };
        let missing = OverlayStamp {
            uid: "gone".into(),
            namespace: "other".into(),
            name: "nope".into(),
            tint: Some(Severity::Ok),
            samples: Vec::new(),
            label: None,
        };
        let frame = resolve_frame(OverlayKind::Policy, &[by_uid, by_name, missing], &snap);
        assert_eq!(frame.kind, Some(OverlayKind::Policy));
        assert_eq!(frame.marks.len(), 2);
        assert_eq!(frame.marks[0].uid, "pod-uid");
        assert_eq!(frame.marks[1].uid, "pod-uid");
        assert!(frame.marks[1].sparkline.is_some());
    }
}
