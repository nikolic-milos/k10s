//! What the user has selected, named well enough to ask the cluster about it.
//!
//! A [`Selection`] is derived from the exact snapshot that was on screen when
//! the click landed, by a pure function, so a panel can never disagree with the
//! frame the user was looking at. The uid is what keys identity across
//! publishes -- slots are reused, and after a delete-and-add the same slot names
//! a different pod with nothing but the uid to say so. The kind id names the API
//! resource a describe needs rather than a display string, and the ancestry is
//! what a data-plane request needs: a pod's logs want its namespace's name, not
//! its slot.

use std::sync::Arc;

use k10s_core::{KindId, Level, SceneSnapshot, SlotIds, kind_short};
use k10s_map::PickPath;

use crate::provider::{DescribeRequest, UsageRequest, UsageTarget, WorkloadLogRequest};

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub level: Level,
    pub kind: &'static str,
    pub kind_id: KindId,
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub namespace: Option<Arc<str>>,
    pub owner: Option<Arc<str>>,
}

impl Selection {
    pub fn from_pick(snapshot: &SceneSnapshot, path: PickPath) -> Option<Selection> {
        let id_of = |ids: &SlotIds, slot: u32| {
            ids.get(slot as usize)
                .cloned()
                .unwrap_or_else(|| Arc::from(""))
        };
        let region = snapshot.regions.get(path.region as usize)?;
        let namespace = region.label.clone();
        let owner = path
            .block
            .and_then(|slot| snapshot.blocks.get(slot as usize))
            .map(|block| block.label.clone());

        let selection = match path.level() {
            Level::Region => Selection {
                level: Level::Region,
                kind: "namespace",
                kind_id: KindId::NAMESPACE,
                uid: id_of(&snapshot.ids.regions, path.region),
                name: namespace,
                namespace: None,
                owner: None,
            },
            Level::Block => {
                let slot = path.block?;
                let block = snapshot.blocks.get(slot as usize)?;
                Selection {
                    level: Level::Block,
                    kind: kind_short(block.ext.kind),
                    kind_id: block.ext.kind,
                    uid: id_of(&snapshot.ids.blocks, slot),
                    name: block.label.clone(),
                    namespace: Some(namespace),
                    owner: None,
                }
            }
            Level::Cell => {
                let slot = path.cell?;
                let cell = snapshot.cells.get(slot as usize)?;
                Selection {
                    level: Level::Cell,
                    kind: "pod",
                    kind_id: KindId::POD,
                    uid: id_of(&snapshot.ids.cells, slot),
                    name: cell.label.clone(),
                    namespace: Some(namespace),
                    owner,
                }
            }
            Level::Sat => {
                let slot = path.sat?;
                let satellite = snapshot.sats.get(slot as usize)?;
                Selection {
                    level: Level::Sat,
                    kind: kind_short(satellite.ext.kind),
                    kind_id: satellite.ext.kind,
                    uid: id_of(&snapshot.ids.sats, slot),
                    name: satellite.label.clone(),
                    namespace: Some(namespace),
                    owner,
                }
            }
        };
        Some(selection)
    }

    pub fn describe_request(&self) -> DescribeRequest {
        DescribeRequest {
            kind: self.kind_id,
            namespace: self.namespace.as_deref().map(str::to_string),
            name: self.name.to_string(),
            uid: self.uid.to_string(),
        }
    }

    // Which log follow this selection names, if any: a pod is one stream, a
    // workload is the merged follows of its pods. Logs on a namespace or an
    // attachment answer None -- doing nothing there is a decision this
    // function owns, not an accident of a listener's match.
    pub fn log_target(&self) -> Option<LogTarget> {
        let namespace = self.namespace.as_deref()?;
        match self.level {
            Level::Cell => Some(LogTarget::Pod {
                namespace: namespace.to_string(),
                name: self.name.to_string(),
            }),
            Level::Block => Some(LogTarget::Workload(WorkloadLogRequest {
                namespace: namespace.to_string(),
                kind: self.kind_id,
                name: self.name.to_string(),
            })),
            Level::Region | Level::Sat => None,
        }
    }

    // Which usage poll this selection names, if any: a pod is one set of
    // numbers, a workload is the sum over the pods its selector matches.
    // Usage on a namespace or an attachment answers None -- doing nothing
    // there is a decision this function owns, exactly like `log_target`.
    pub fn usage_target(&self) -> Option<UsageRequest> {
        let namespace = self.namespace.as_deref()?;
        let target = match self.level {
            Level::Cell => UsageTarget::Pod {
                name: self.name.to_string(),
            },
            Level::Block => UsageTarget::Workload {
                kind: self.kind_id,
                name: self.name.to_string(),
            },
            Level::Region | Level::Sat => return None,
        };
        Some(UsageRequest {
            namespace: namespace.to_string(),
            target,
        })
    }

    // The pod this selection is, if it is one: what an exec needs before any
    // transport policy gets involved. `from_pick` always gives a Cell its
    // namespace, so the None arm here is defensive; stating it once beats
    // re-guarding it in every listener.
    pub fn pod(&self) -> Option<(String, String)> {
        let namespace = self.namespace.as_deref()?;
        (self.level == Level::Cell).then(|| (namespace.to_string(), self.name.to_string()))
    }
}

// A log follow the shell can open, named by the selection that asked for it.
// Transport details a request carries beyond identity -- a pod follow's
// container and `previous` flag -- stay with the opener, which is why the Pod
// arm is not a `LogRequest`.
#[derive(Debug, Clone, PartialEq)]
pub enum LogTarget {
    Pod { namespace: String, name: String },
    Workload(WorkloadLogRequest),
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{Op, State, replay};
    use k10s_world::{LayoutMode, PublishBench};

    fn slot_of(labels: impl Iterator<Item = Arc<str>>, name: &str) -> u32 {
        labels
            .enumerate()
            .find(|(_, label)| label.as_ref() == name)
            .map(|(slot, _)| slot as u32)
            .expect("the fixture names exist")
    }

    #[test]
    fn a_pick_becomes_a_selection_the_data_plane_can_act_on() {
        let initial = replay::initial_sync();
        let bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();

        let region = slot_of(snapshot.regions.iter().map(|n| n.label.clone()), "prod");
        let block = slot_of(snapshot.blocks.iter().map(|n| n.label.clone()), "api");
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");

        let selection = Selection::from_pick(
            &snapshot,
            PickPath {
                region,
                block: Some(block),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("the path names live slots");
        assert_eq!(selection.level, Level::Cell);
        assert_eq!(selection.kind, "pod");
        assert_eq!(
            selection.kind_id,
            KindId::POD,
            "a describe needs the API resource, not a display string"
        );
        assert_eq!(selection.name.as_ref(), "pod-1");
        assert_eq!(
            selection.uid.as_ref(),
            "pod-1",
            "the uid comes from the identity vectors, not the label"
        );
        assert_eq!(selection.namespace.as_deref(), Some("prod"));
        assert_eq!(selection.owner.as_deref(), Some("api"));

        let request = selection.describe_request();
        assert_eq!(request.kind, KindId::POD);
        assert_eq!(request.namespace.as_deref(), Some("prod"));
        assert_eq!(request.name, "pod-1");
        assert_eq!(request.uid, "pod-1");

        let namespace_only = Selection::from_pick(
            &snapshot,
            PickPath {
                region,
                block: None,
                cell: None,
                sat: None,
            },
        )
        .expect("a region pick resolves");
        assert_eq!(namespace_only.level, Level::Region);
        assert_eq!(namespace_only.kind, "namespace");
        assert_eq!(namespace_only.kind_id, KindId::NAMESPACE);
        assert_eq!(namespace_only.uid.as_ref(), "ns-prod");
        assert_eq!(namespace_only.namespace, None);
        assert_eq!(
            namespace_only.describe_request().namespace,
            None,
            "a cluster-scoped describe carries no namespace"
        );
    }

    #[test]
    fn a_selection_keyed_by_uid_sees_slot_reuse() {
        let initial = replay::initial_sync();
        let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");
        let before = Selection::from_pick(
            &snapshot,
            PickPath {
                region: 0,
                block: Some(0),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("pod-1 resolves");
        drop(snapshot);

        bench.apply_events(&[
            replay::instance("pod-1", "prod", "wl-api", State::OK, Op::Deleted),
            replay::instance("pod-intruder", "prod", "wl-api", State::OK, Op::Added),
        ]);
        bench.run_publish();
        let snapshot = bench.snapshot();
        let after = Selection::from_pick(
            &snapshot,
            PickPath {
                region: 0,
                block: Some(0),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("the reused slot resolves");
        assert_eq!(after.name.as_ref(), "pod-intruder");
        assert_ne!(
            before.uid, after.uid,
            "the same slot now names a different pod, and only the uid says so"
        );
    }

    #[test]
    fn a_slot_with_no_identity_selects_with_an_empty_uid() {
        // Tombstoned slots hold the empty string, and an identity vector shorter
        // than the node vector it parallels means the same thing. The empty uid
        // is not a placeholder: `SceneSnapshot::locate` refuses to search for it
        // rather than matching every hole in the scene, and the inspector leaves
        // the UID row out entirely. Anything else invented here would be a uid
        // that names nothing and looks like it names something.
        let initial = replay::initial_sync();
        let bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");

        let mut ids = (*snapshot.ids).clone();
        ids.cells.truncate(cell as usize);
        let mut starved = (*snapshot).clone();
        starved.ids = std::sync::Arc::new(ids);

        let selection = Selection::from_pick(
            &starved,
            PickPath {
                region: 0,
                block: Some(0),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("the slot is still a live pod, it just has no identity");
        assert_eq!(selection.name.as_ref(), "pod-1");
        assert_eq!(
            selection.uid.as_ref(),
            "",
            "a slot with no identity gets the empty uid, not a stand-in"
        );
        assert_eq!(
            selection.describe_request().uid,
            "",
            "and the request carries it through rather than inventing one"
        );
        assert!(
            starved.locate(&selection.uid).is_none(),
            "an empty uid must find nothing, which is what makes it safe to pass on"
        );
    }

    #[test]
    fn a_pick_naming_a_slot_that_is_gone_resolves_to_nothing() {
        // Picks travel with the snapshot they were taken against, but a path
        // built by hand -- or replayed after a shrink -- can name a slot that no
        // longer exists. Answering `None` is what keeps a panel from opening on
        // a resource nobody clicked.
        let initial = replay::initial_sync();
        let bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();

        assert!(
            Selection::from_pick(
                &snapshot,
                PickPath {
                    region: u32::MAX,
                    block: None,
                    cell: None,
                    sat: None,
                },
            )
            .is_none(),
            "a region slot past the end names no namespace"
        );
        assert!(
            Selection::from_pick(
                &snapshot,
                PickPath {
                    region: 0,
                    block: Some(u32::MAX),
                    cell: None,
                    sat: None,
                },
            )
            .is_none(),
            "a block slot past the end names no workload"
        );
        assert!(
            Selection::from_pick(
                &snapshot,
                PickPath {
                    region: 0,
                    block: Some(0),
                    cell: Some(u32::MAX),
                    sat: None,
                },
            )
            .is_none(),
            "a cell slot past the end names no pod"
        );
    }

    #[test]
    fn a_selection_names_the_log_follow_it_wants_or_none_at_all() {
        let initial = replay::initial_sync();
        let bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();
        let region = slot_of(snapshot.regions.iter().map(|n| n.label.clone()), "prod");
        let block = slot_of(snapshot.blocks.iter().map(|n| n.label.clone()), "api");
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");
        let pick = |b, c| {
            Selection::from_pick(
                &snapshot,
                PickPath {
                    region,
                    block: b,
                    cell: c,
                    sat: None,
                },
            )
            .expect("the path names live slots")
        };

        let pod = pick(Some(block), Some(cell));
        assert_eq!(
            pod.log_target(),
            Some(LogTarget::Pod {
                namespace: "prod".to_string(),
                name: "pod-1".to_string(),
            })
        );
        assert_eq!(
            pod.pod(),
            Some(("prod".to_string(), "pod-1".to_string())),
            "an exec starts from the same identity a log follow does"
        );

        let workload = pick(Some(block), None);
        match workload.log_target() {
            Some(LogTarget::Workload(request)) => {
                assert_eq!(request.namespace, "prod");
                assert_eq!(request.name, "api");
                assert_eq!(request.kind, workload.kind_id);
            }
            other => panic!("a workload names the merged follow: {other:?}"),
        }
        assert_eq!(workload.pod(), None, "a workload is not a pod to exec into");

        let namespace = pick(None, None);
        assert_eq!(
            namespace.log_target(),
            None,
            "logs on a namespace do nothing, as a decision rather than an accident"
        );
        assert_eq!(namespace.pod(), None);
    }
}
