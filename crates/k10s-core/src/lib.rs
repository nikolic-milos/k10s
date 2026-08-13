//! The open model and the ingestion contract every other crate meets at.
//!
//! Kinds, tools, and reasons are interned dense ids (`KindId`, `ToolId`,
//! `ReasonId`) resolved through a `Catalog`, never strings in hot paths. The
//! scene is four levels of role (scope, owner, instance, satellite) held in
//! flat vectors inside `SceneSnapshot`; a published snapshot is immutable, so
//! a reader holding one must never observe a mutation. Ingestion is an event
//! stream (`IngestEvent`), not a snapshot type, and `Intake` bounds it on both
//! axes -- it coalesces by object uid and degrades to a labelled `Desync`
//! resync instead of blocking or growing.

pub mod ingest;
pub mod layout;
pub mod model;
mod prepared;
pub mod replay;

use std::fmt;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use arc_swap::ArcSwap;

pub use ingest::{
    CONTROL_CAPACITY, Capability, DEFAULT_INTAKE_CAPACITY, DesyncReason, IngestEvent, Intake,
    IntakeStats, Op, Payload, ResourceEvent,
};
pub use k10s_atlas::{
    BlockNode, CellNode, Edge, EdgeIndex, Endpoint, Level, Rect, RegionNode, Scene, Totals,
};
pub use model::{
    BUILTIN_KIND_COUNT, BUILTIN_KINDS, BUILTIN_REASON_COUNT, BUILTIN_REASONS, BUILTIN_TOOL_COUNT,
    BUILTIN_TOOLS, Catalog, KindEntry, KindId, KindInfo, ReasonId, ReasonInfo, Role, Severity,
    State, ToolId, ToolInfo, kind_role, kind_short, reason_severity,
};
pub use prepared::{PreparedNamespace, PreparedPod, PreparedSat, PreparedScene, PreparedWorkload};
pub use replay::RecordedStream;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NsExt {
    pub unhealthy_frac: f32,
    pub rollup: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WlExt {
    pub kind: KindId,
    pub tool: ToolId,
    pub rollup: Severity,
    pub ns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PodExt {
    pub state: State,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SatExt {
    pub kind: KindId,
    pub detail: Arc<str>,
}

pub type NsNode = RegionNode<NsExt>;
pub type WorkloadNode = BlockNode<WlExt>;
pub type PodNode = CellNode<PodExt>;
pub type SatNode = CellNode<SatExt>;

pub type EdgeInst = Edge;

pub type SceneData = Scene<NsExt, WlExt, PodExt, SatExt>;

const SLOT_ID_PAGE_LEN: usize = 1_024;

/// Slot-ordered immutable identities with page-granular copy-on-write.
///
/// A million-object scene used to clone a million `Arc<str>` values into every
/// newly materialized snapshot even though topology already owned the same
/// immutable strings in the same order. `SlotIds` shares the original flat
/// vector instead. A live insertion or reuse overlays only the affected
/// 1,024-entry page, so held snapshots remain immutable without cloning the
/// million-entry base. The common initial scene stays contiguous for both
/// construction and search.
///
/// Tombstoned slots hold the empty string, just as the former flat vectors did.
#[derive(Clone, Default)]
pub struct SlotIds {
    base: Arc<Vec<Arc<str>>>,
    overrides: Vec<Option<Arc<Vec<Arc<str>>>>>,
    len: usize,
}

impl SlotIds {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            base: Arc::new(Vec::with_capacity(capacity)),
            overrides: Vec::new(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<&Arc<str>> {
        if index >= self.len {
            return None;
        }
        let page = index / SLOT_ID_PAGE_LEN;
        let offset = index % SLOT_ID_PAGE_LEN;
        match self.overrides.get(page).and_then(Option::as_deref) {
            Some(values) => values.get(offset),
            None => self.base.get(index),
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Arc<str>> {
        if index >= self.len {
            return None;
        }
        if self.overrides.is_empty() && Arc::strong_count(&self.base) == 1 {
            return Arc::get_mut(&mut self.base)
                .expect("the identity base is uniquely owned")
                .get_mut(index);
        }

        let page = index / SLOT_ID_PAGE_LEN;
        let offset = index % SLOT_ID_PAGE_LEN;
        if self.overrides.len() <= page {
            self.overrides.resize_with(page + 1, || None);
        }
        let values = self.overrides[page].get_or_insert_with(|| {
            let start = page * SLOT_ID_PAGE_LEN;
            let end = (start + SLOT_ID_PAGE_LEN)
                .min(self.len)
                .min(self.base.len());
            let mut values = Vec::with_capacity(SLOT_ID_PAGE_LEN);
            values.extend(self.base[start..end].iter().cloned());
            Arc::new(values)
        });
        let base_len = self.base.len();
        let values = Arc::make_mut(values);
        assert!(
            offset < values.len(),
            "slot identity page is incomplete: index={index} len={} base_len={base_len} page={page} offset={offset} page_len={}",
            self.len,
            values.len(),
        );
        Some(&mut values[offset])
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<str>> {
        if self.overrides.is_empty() {
            SlotIdsIter::Base(self.base[..self.len].iter())
        } else {
            SlotIdsIter::Indexed { ids: self, next: 0 }
        }
    }

    pub fn push(&mut self, uid: Arc<str>) {
        if self.overrides.is_empty()
            && let Some(base) = Arc::get_mut(&mut self.base)
        {
            base.truncate(self.len);
            base.push(uid);
            self.len += 1;
            return;
        }

        let page = self.len / SLOT_ID_PAGE_LEN;
        let offset = self.len % SLOT_ID_PAGE_LEN;
        if self.overrides.len() <= page {
            self.overrides.resize_with(page + 1, || None);
        }
        let values = self.overrides[page].get_or_insert_with(|| {
            let start = page * SLOT_ID_PAGE_LEN;
            let end = (start + offset).min(self.base.len());
            let mut values = Vec::with_capacity(SLOT_ID_PAGE_LEN);
            if start < end {
                values.extend(self.base[start..end].iter().cloned());
            }
            Arc::new(values)
        });
        let values = Arc::make_mut(values);
        values.truncate(offset);
        debug_assert_eq!(values.len(), offset);
        values.push(uid);
        self.len += 1;
    }

    pub fn clear(&mut self) {
        self.base = Arc::new(Vec::new());
        self.overrides.clear();
        self.len = 0;
    }

    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        if self.overrides.is_empty()
            && let Some(base) = Arc::get_mut(&mut self.base)
        {
            base.truncate(len);
            self.len = len;
            return;
        }

        self.len = len;
        self.overrides.truncate(len.div_ceil(SLOT_ID_PAGE_LEN));
        let tail_len = len % SLOT_ID_PAGE_LEN;
        if tail_len > 0
            && let Some(Some(values)) = self.overrides.last_mut()
        {
            Arc::make_mut(values).truncate(tail_len);
        }
    }
}

enum SlotIdsIter<'a> {
    Base(std::slice::Iter<'a, Arc<str>>),
    Indexed { ids: &'a SlotIds, next: usize },
}

impl<'a> Iterator for SlotIdsIter<'a> {
    type Item = &'a Arc<str>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SlotIdsIter::Base(iter) => iter.next(),
            SlotIdsIter::Indexed { ids, next } => {
                let value = ids.get(*next)?;
                *next += 1;
                Some(value)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for SlotIdsIter<'_> {
    fn len(&self) -> usize {
        match self {
            SlotIdsIter::Base(iter) => iter.len(),
            SlotIdsIter::Indexed { ids, next } => ids.len - *next,
        }
    }
}

impl Extend<Arc<str>> for SlotIds {
    fn extend<T: IntoIterator<Item = Arc<str>>>(&mut self, iter: T) {
        for uid in iter {
            self.push(uid);
        }
    }
}

impl FromIterator<Arc<str>> for SlotIds {
    fn from_iter<T: IntoIterator<Item = Arc<str>>>(iter: T) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl From<Vec<Arc<str>>> for SlotIds {
    fn from(ids: Vec<Arc<str>>) -> Self {
        let len = ids.len();
        Self {
            base: Arc::new(ids),
            overrides: Vec::new(),
            len,
        }
    }
}

impl Index<usize> for SlotIds {
    type Output = Arc<str>;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .unwrap_or_else(|| panic!("slot id index {index} out of bounds for {} ids", self.len()))
    }
}

impl IndexMut<usize> for SlotIds {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        let len = self.len();
        self.get_mut(index)
            .unwrap_or_else(|| panic!("slot id index {index} out of bounds for {len} ids"))
    }
}

impl PartialEq for SlotIds {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for SlotIds {}

impl fmt::Debug for SlotIds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

/// Opaque per-slot identities, parallel to the scene's node vectors.
///
/// The engine below never reads them: they exist so a consumer holding a
/// snapshot can say what a slot *is* -- selection, data requests -- across
/// publishes, where slot reuse would otherwise let a bare index silently change
/// meaning. Tombstoned slots hold the empty string.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneIds {
    pub regions: SlotIds,
    pub blocks: SlotIds,
    pub cells: SlotIds,
    pub sats: SlotIds,
}

/// The scene the engine draws plus the identity the model layer needs, one
/// value so both swap atomically under the same Arc.
///
/// Identity lives here and not on `Scene` deliberately: the engine's hot type
/// stays engine-only, and the ids cost one reference bump per snapshot clone.
#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub scene: SceneData,
    pub ids: Arc<SceneIds>,
}

/// Where an object is, for anything that has a uid and wants a place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Located {
    pub level: Level,
    pub slot: usize,
    pub rect: Rect,
}

impl SceneSnapshot {
    /// Find an object by the uid it was published under.
    ///
    /// Searched deepest-first, which costs nothing and settles a question the
    /// caller should not have to: uids are unique across the cluster, so a hit at
    /// two levels would mean the identity vectors disagree with themselves, and
    /// answering with the most specific one is the answer a person searching for
    /// a name wants either way.
    ///
    /// A tombstoned slot holds the empty string, so an empty query finds nothing
    /// rather than finding every hole in the scene -- which is the one input a
    /// search box produces constantly.
    pub fn locate(&self, uid: &str) -> Option<Located> {
        if uid.is_empty() {
            return None;
        }
        let find = |ids: &SlotIds| ids.iter().position(|id| id.as_ref() == uid);
        if let Some(slot) = find(&self.ids.sats)
            && let Some(node) = self.scene.sats.get(slot)
        {
            return Some(Located {
                level: Level::Sat,
                slot,
                rect: node.rect,
            });
        }
        if let Some(slot) = find(&self.ids.cells)
            && let Some(node) = self.scene.cells.get(slot)
        {
            return Some(Located {
                level: Level::Cell,
                slot,
                rect: node.rect,
            });
        }
        if let Some(slot) = find(&self.ids.blocks)
            && let Some(node) = self.scene.blocks.get(slot)
        {
            // The card, not the halo: the halo is spacing, and framing it puts a
            // ring of empty space where the context should be. The same
            // distinction picking makes.
            return Some(Located {
                level: Level::Block,
                slot,
                rect: node.inner,
            });
        }
        if let Some(slot) = find(&self.ids.regions)
            && let Some(node) = self.scene.regions.get(slot)
        {
            return Some(Located {
                level: Level::Region,
                slot,
                rect: node.rect,
            });
        }
        None
    }
}

impl std::ops::Deref for SceneSnapshot {
    type Target = SceneData;

    fn deref(&self) -> &SceneData {
        &self.scene
    }
}

impl std::ops::DerefMut for SceneSnapshot {
    fn deref_mut(&mut self) -> &mut SceneData {
        &mut self.scene
    }
}

pub type SharedScene = Arc<ArcSwap<SceneSnapshot>>;

pub fn new_shared_scene() -> SharedScene {
    Arc::new(ArcSwap::from_pointee(SceneSnapshot::default()))
}

#[derive(Debug)]
pub enum WorldCtrl {
    SetChurn(bool),
    /// Flips per second the synthetic churn is allowed to spend. Set after
    /// spawn because the scene's provenance is chosen on screen now: a world
    /// that was still empty when it started must be able to learn that what
    /// arrived is a real cluster, where inventing pod transitions would be a
    /// lie, or the generator, where they are the point.
    SetChurnRate(f32),
    /// Replace the whole scene with one built from this stream: what a cluster
    /// chosen on screen sends.
    ///
    /// The stream travels *with* the instruction rather than down the event
    /// channel behind it, for two reasons. A scene arriving all at once has to
    /// be laid out the way the command line's scenes are, by the batch layout --
    /// the incremental one exists for the namespace that appears at runtime, and
    /// placing two hundred of them one after another produces a strip. And a
    /// reset sent alongside the events it replaces would race them: control and
    /// events are separate channels read at different points in a tick, so the
    /// old scene would be re-applied on top of the new one from whatever was
    /// still queued. Carrying the stream makes the replacement one act.
    Rebuild(Vec<IngestEvent>),
    /// Replace the whole scene from an already hierarchical batch. Synthetic
    /// sources can produce this directly instead of allocating an event per
    /// object only for the world to fold those events back into a hierarchy.
    RebuildPrepared(PreparedScene),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(index: usize) -> Arc<str> {
        Arc::from(format!("uid-{index}"))
    }

    #[test]
    fn slot_ids_clone_shares_the_base_and_overlays_only_the_touched_page() {
        let mut original = SlotIds::with_capacity(SLOT_ID_PAGE_LEN * 2 + 7);
        original.extend((0..SLOT_ID_PAGE_LEN * 2 + 7).map(uid));
        let mut changed = original.clone();

        assert_eq!(original.len(), SLOT_ID_PAGE_LEN * 2 + 7);
        assert!(Arc::ptr_eq(&original.base, &changed.base));
        assert!(original.overrides.is_empty());
        assert!(changed.overrides.is_empty());

        changed[SLOT_ID_PAGE_LEN + 3] = Arc::from("replacement");

        assert!(Arc::ptr_eq(&original.base, &changed.base));
        assert!(original.overrides.is_empty());
        assert!(changed.overrides[0].is_none());
        assert!(changed.overrides[1].is_some());
        assert_eq!(
            original[SLOT_ID_PAGE_LEN + 3].as_ref(),
            format!("uid-{}", SLOT_ID_PAGE_LEN + 3)
        );
        assert_eq!(changed[SLOT_ID_PAGE_LEN + 3].as_ref(), "replacement");
        assert_eq!(original[original.len() - 1], changed[changed.len() - 1]);

        let appended = changed.len();
        changed.push(Arc::from("appended"));
        changed[3] = Arc::from("front-page-change");
        assert_eq!(changed[appended].as_ref(), "appended");
        assert_eq!(changed[3].as_ref(), "front-page-change");
        assert_eq!(original[3].as_ref(), "uid-3");
    }

    #[test]
    fn slot_ids_truncate_preserves_flat_vector_semantics_across_a_page_boundary() {
        let mut ids: SlotIds = (0..SLOT_ID_PAGE_LEN + 9).map(uid).collect();
        ids.truncate(SLOT_ID_PAGE_LEN - 3);
        assert_eq!(ids.len(), SLOT_ID_PAGE_LEN - 3);
        assert_eq!(
            ids[ids.len() - 1].as_ref(),
            format!("uid-{}", SLOT_ID_PAGE_LEN - 4)
        );
        assert!(ids.get(ids.len()).is_none());

        ids.push(Arc::from("after-truncate"));
        assert_eq!(ids.len(), SLOT_ID_PAGE_LEN - 2);
        assert_eq!(ids[ids.len() - 1].as_ref(), "after-truncate");
    }

    #[test]
    fn slot_ids_regrow_past_a_page_the_clone_had_overlaid_and_then_truncated_away() {
        let original: SlotIds = (0..SLOT_ID_PAGE_LEN * 2 + 7).map(uid).collect();
        let mut changed = original.clone();
        changed[SLOT_ID_PAGE_LEN + 3] = Arc::from("second-page-change");

        changed.truncate(SLOT_ID_PAGE_LEN - 5);
        changed.push(Arc::from("regrown"));
        changed[0] = Arc::from("first-page-change");
        while changed.len() <= SLOT_ID_PAGE_LEN {
            let next = uid(changed.len() + 100_000);
            changed.push(next);
        }
        let across = uid(SLOT_ID_PAGE_LEN + 100_000);
        assert_eq!(
            changed[SLOT_ID_PAGE_LEN], across,
            "the page the clone had overlaid was truncated away, so regrowing \
             must not resurrect what that overlay held"
        );
        changed[SLOT_ID_PAGE_LEN] = Arc::from("across-the-boundary");

        assert_eq!(changed[0].as_ref(), "first-page-change");
        assert_eq!(
            changed[SLOT_ID_PAGE_LEN - 6].as_ref(),
            format!("uid-{}", SLOT_ID_PAGE_LEN - 6)
        );
        assert_eq!(changed[SLOT_ID_PAGE_LEN - 5].as_ref(), "regrown");
        assert_eq!(changed[SLOT_ID_PAGE_LEN].as_ref(), "across-the-boundary");
        assert_eq!(changed.len(), SLOT_ID_PAGE_LEN + 1);

        assert_eq!(original.len(), SLOT_ID_PAGE_LEN * 2 + 7);
        assert_eq!(original[0].as_ref(), "uid-0");
        assert_eq!(
            original[SLOT_ID_PAGE_LEN + 3].as_ref(),
            format!("uid-{}", SLOT_ID_PAGE_LEN + 3)
        );
    }

    #[test]
    fn node_extension_strides_are_pinned() {
        assert_eq!(size_of::<NsExt>(), 8, "NsExt");
        assert_eq!(size_of::<WlExt>(), 12, "WlExt");
        assert_eq!(size_of::<PodExt>(), 8, "PodExt");
        assert_eq!(size_of::<SatExt>(), 24, "SatExt");
        assert_eq!(size_of::<State>(), 8, "State");
        assert_eq!(size_of::<Severity>(), 1, "Severity is the rollup axis");

        assert_eq!(size_of::<NsNode>(), 56, "NsNode");
        assert_eq!(size_of::<WorkloadNode>(), 80, "WorkloadNode");
        assert_eq!(size_of::<PodNode>(), 40, "PodNode");
        assert_eq!(size_of::<SatNode>(), 56, "SatNode");
    }
}

#[cfg(test)]
mod locate_tests {
    use super::*;

    fn snapshot() -> SceneSnapshot {
        let mut snap = SceneSnapshot::default();
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
        snap.scene.blocks.push(WorkloadNode {
            rect: Rect::new(10.0, 10.0, 120.0, 90.0),
            inner: Rect::new(14.0, 14.0, 112.0, 82.0),
            label: Arc::from("api"),
            children: 0..1,
            sats: 0..1,
            ext: WlExt {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                rollup: Severity::Ok,
                ns: 0,
            },
        });
        snap.scene.cells.push(PodNode {
            rect: Rect::new(20.0, 30.0, 16.0, 16.0),
            label: Arc::from("api-0"),
            ext: PodExt { state: State::OK },
        });
        snap.scene.sats.push(SatNode {
            rect: Rect::new(140.0, 40.0, 12.0, 12.0),
            label: Arc::from("svc"),
            ext: SatExt {
                kind: KindId::SERVICE,
                detail: Arc::from("80/TCP"),
            },
        });
        let ids = Arc::make_mut(&mut snap.ids);
        ids.regions.push(Arc::from("ns-prod"));
        ids.blocks.push(Arc::from("wl-api"));
        ids.cells.push(Arc::from("pod-0"));
        ids.sats.push(Arc::from("svc-api"));
        snap
    }

    #[test]
    fn every_level_is_findable_by_the_uid_it_was_published_under() {
        let snap = snapshot();
        for (uid, level) in [
            ("ns-prod", Level::Region),
            ("wl-api", Level::Block),
            ("pod-0", Level::Cell),
            ("svc-api", Level::Sat),
        ] {
            let found = snap
                .locate(uid)
                .unwrap_or_else(|| panic!("{uid} was not found"));
            assert_eq!(found.level, level, "{uid}");
            assert_eq!(found.slot, 0);
        }
        assert!(snap.locate("nothing-here").is_none());
    }

    #[test]
    fn a_workload_locates_to_its_card_and_not_to_its_halo() {
        let snap = snapshot();
        let found = snap.locate("wl-api").expect("the workload is there");
        assert_eq!(found.rect, snap.scene.blocks[0].inner);
        assert_ne!(
            found.rect, snap.scene.blocks[0].rect,
            "the halo and the card must differ or this assertion is empty"
        );
    }

    #[test]
    fn the_empty_uid_finds_nothing_rather_than_every_tombstone() {
        let mut snap = snapshot();
        // A tombstoned slot: the id is cleared, the node stays.
        Arc::make_mut(&mut snap.ids).cells[0] = Arc::from("");
        assert!(
            snap.locate("").is_none(),
            "an empty query matched a hole in the scene, which is what a search \
             box sends on every keystroke that deletes the last character"
        );
        assert!(
            snap.locate("pod-0").is_none(),
            "a tombstoned slot is not findable"
        );
    }

    #[test]
    fn a_snapshot_whose_ids_outrun_its_nodes_answers_none_instead_of_panicking() {
        // Identity and geometry are published together, but they are two vectors,
        // and a caller holding a snapshot mid-rebuild must not index off the end
        // of one because the other was longer.
        let mut snap = snapshot();
        Arc::make_mut(&mut snap.ids).cells.push(Arc::from("pod-1"));
        assert!(snap.locate("pod-1").is_none());
    }
}
