//! Finding a thing on the map by name and published scene fields.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use k10s_core::{BUILTIN_KINDS, BUILTIN_REASONS, KindId, Level, ReasonId, SceneSnapshot, Severity};
use k10s_map::{OverlayFrame, OverlayKind};

use crate::palette::fuzzy_score;

/// One thing on the map that can be found by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What the map is asked to reveal. Never the slot: slots are reused, and a
    /// search that outlives a publish would otherwise reveal whatever moved in.
    pub uid: Arc<str>,
    /// What is matched and what is shown, which must be the same string or the
    /// highlighted ranges point into text nobody is reading. `namespace/name`,
    /// so typing a namespace narrows to it without a separate filter.
    pub label: String,
    pub level: Level,
}

#[derive(Debug, Clone, Copy)]
struct CandidateMeta {
    slot: u32,
}

/// One hit, with where the query matched so the row can show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub candidate: usize,
    pub score: i64,
    pub hits: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPredicate {
    Severity(Severity),
    Unhealthy,
    Reason(ReasonId),
}

#[derive(Debug, Default)]
struct OverlayIndex {
    kind: Option<OverlayKind>,
    uids: HashSet<String>,
}

impl OverlayIndex {
    fn from_frame(frame: &OverlayFrame) -> OverlayIndex {
        OverlayIndex {
            kind: frame.kind,
            uids: frame.marks.iter().map(|mark| mark.uid.clone()).collect(),
        }
    }

    fn stamps(&self, kind: OverlayKind, uid: &str) -> bool {
        self.kind == Some(kind) && self.uids.contains(uid)
    }
}

#[derive(Debug, Default)]
struct CompiledQuery {
    fuzzy: String,
    kinds: Vec<KindId>,
    namespaces: Vec<String>,
    statuses: Vec<StatusPredicate>,
    uids: Vec<String>,
    overlays: Vec<OverlayKind>,
}

impl CompiledQuery {
    fn compile(input: &str) -> Result<Self, String> {
        let mut compiled = CompiledQuery::default();
        let mut fuzzy_terms = Vec::new();

        for term in input.split_whitespace() {
            if let Some((qualifier, value)) = term.split_once(':') {
                if value.is_empty() {
                    return Err(format!("{qualifier}: needs a value"));
                }
                match qualifier.to_ascii_lowercase().as_str() {
                    "kind" => compiled.kinds.push(
                        parse_kind(value)
                            .ok_or_else(|| format!("unknown built-in Kubernetes kind: {value}"))?,
                    ),
                    "ns" | "namespace" => compiled.namespaces.push(value.to_string()),
                    "status" => compiled.statuses.push(
                        parse_status(value)
                            .ok_or_else(|| format!("unknown built-in status: {value}"))?,
                    ),
                    "uid" => compiled.uids.push(value.to_string()),
                    "overlay" => compiled.overlays.push(
                        OverlayKind::parse(value)
                            .ok_or_else(|| format!("unknown overlay: {value}"))?,
                    ),
                    "label" => {
                        // A SceneSnapshot has names and health, not Kubernetes
                        // labels, so this qualifier cannot become a silent fuzzy.
                        return Err("label: is not on the published scene".to_string());
                    }
                    _ => return Err(format!("unsupported qualifier: {qualifier}:")),
                }
                continue;
            }

            if let Some(reason) = parse_reason(term) {
                compiled.statuses.push(StatusPredicate::Reason(reason));
            } else {
                fuzzy_terms.push(term);
            }
        }
        compiled.fuzzy = fuzzy_terms.join(" ");
        Ok(compiled)
    }

    fn matches(
        &self,
        candidate: &Candidate,
        meta: CandidateMeta,
        snapshot: &SceneSnapshot,
        overlay: &OverlayIndex,
    ) -> bool {
        self.kinds
            .iter()
            .all(|kind| Some(*kind) == meta.kind(candidate.level, snapshot))
            && self
                .namespaces
                .iter()
                .all(|namespace| namespace == meta.namespace(candidate))
            && self.uids.iter().all(|uid| uid == candidate.uid.as_ref())
            && self.statuses.iter().all(|predicate| {
                meta.status(candidate.level, snapshot)
                    .is_some_and(|status| predicate.matches(status))
            })
            && (self.overlays.is_empty()
                || self
                    .overlays
                    .iter()
                    .any(|kind| overlay.stamps(*kind, candidate.uid.as_ref())))
    }

    fn is_unfiltered(&self) -> bool {
        self.fuzzy.is_empty()
            && self.kinds.is_empty()
            && self.namespaces.is_empty()
            && self.statuses.is_empty()
            && self.uids.is_empty()
            && self.overlays.is_empty()
    }
}

impl StatusPredicate {
    fn matches(self, status: CandidateStatus) -> bool {
        match self {
            StatusPredicate::Severity(severity) => status.severity == severity,
            StatusPredicate::Unhealthy => status.severity.is_unhealthy(),
            StatusPredicate::Reason(reason) => status.reason == Some(reason),
        }
    }
}

#[derive(Clone, Copy)]
struct CandidateStatus {
    severity: Severity,
    reason: Option<ReasonId>,
}

impl CandidateMeta {
    fn namespace(self, candidate: &Candidate) -> &str {
        candidate
            .label
            .split_once('/')
            .map_or(candidate.label.as_str(), |(namespace, _)| namespace)
    }

    fn kind(self, level: Level, snapshot: &SceneSnapshot) -> Option<KindId> {
        match level {
            Level::Region => Some(KindId::NAMESPACE),
            Level::Block => snapshot
                .blocks
                .get(self.slot as usize)
                .map(|node| node.ext.kind),
            Level::Cell => Some(KindId::POD),
            Level::Sat => snapshot
                .sats
                .get(self.slot as usize)
                .map(|node| node.ext.kind),
        }
    }

    fn status(self, level: Level, snapshot: &SceneSnapshot) -> Option<CandidateStatus> {
        let slot = self.slot as usize;
        match level {
            Level::Region => snapshot.regions.get(slot).map(|node| CandidateStatus {
                severity: node.ext.rollup,
                reason: None,
            }),
            Level::Block => snapshot.blocks.get(slot).map(|node| CandidateStatus {
                severity: node.ext.rollup,
                reason: None,
            }),
            Level::Cell => snapshot.cells.get(slot).map(|node| CandidateStatus {
                severity: node.ext.state.severity,
                reason: Some(node.ext.state.reason),
            }),
            // Satellites publish no independent health state. Borrowing their
            // owner's rollup would make a status query claim data not on them.
            Level::Sat => None,
        }
    }
}

fn parse_kind(value: &str) -> Option<KindId> {
    BUILTIN_KINDS
        .iter()
        .position(|kind| {
            kind.slug.eq_ignore_ascii_case(value)
                || kind.short.eq_ignore_ascii_case(value)
                || kind.kind.eq_ignore_ascii_case(value)
        })
        .map(|index| KindId(index as u32))
}

fn parse_reason(value: &str) -> Option<ReasonId> {
    BUILTIN_REASONS
        .iter()
        .position(|reason| {
            reason.slug.eq_ignore_ascii_case(value) || reason.display.eq_ignore_ascii_case(value)
        })
        .map(|index| ReasonId(index as u32))
}

fn parse_status(value: &str) -> Option<StatusPredicate> {
    let predicate = match value.to_ascii_lowercase().as_str() {
        "ok" | "healthy" => StatusPredicate::Severity(Severity::Ok),
        "unknown" => StatusPredicate::Severity(Severity::Unknown),
        "warn" | "warning" => StatusPredicate::Severity(Severity::Warn),
        "err" | "error" | "critical" => StatusPredicate::Severity(Severity::Err),
        "unhealthy" => StatusPredicate::Unhealthy,
        _ => StatusPredicate::Reason(parse_reason(value)?),
    };
    Some(predicate)
}

/// Everything on the map that can be searched, built once per identity revision.
#[derive(Debug, Default)]
pub struct MapIndex {
    candidates: Vec<Candidate>,
    metadata: Vec<CandidateMeta>,
}

impl MapIndex {
    /// Walk a published scene into a searchable list.
    ///
    /// Tombstoned slots hold the empty uid and are skipped: a hole in the scene
    /// is not an object, and a row that reveals nothing is worse than no row.
    /// Regions come first and satellites last so that equal scores fall out in a
    /// useful order without a second sort key doing the work -- see `rank`.
    pub fn build(snapshot: &SceneSnapshot) -> MapIndex {
        let ids = &snapshot.ids;
        let mut candidates = Vec::with_capacity(
            snapshot.regions.len()
                + snapshot.blocks.len()
                + snapshot.cells.len()
                + snapshot.sats.len(),
        );
        let mut metadata = Vec::with_capacity(candidates.capacity());
        let named = |uid: Option<&Arc<str>>| uid.filter(|uid| !uid.is_empty()).cloned();

        for (slot, node) in snapshot.regions.iter().enumerate() {
            if let Some(uid) = named(ids.regions.get(slot)) {
                candidates.push(Candidate {
                    uid,
                    label: node.label.to_string(),
                    level: Level::Region,
                });
                metadata.push(CandidateMeta { slot: slot as u32 });
            }
        }
        // A workload and a pod carry only their own name, so the namespace is
        // prefixed here rather than left to the person to remember: two clusters
        // in one window will both have an `api`, and "which api" is the question
        // a bare name cannot answer.
        let scope_of = |region: usize| -> &str {
            snapshot
                .regions
                .get(region)
                .map_or("", |node| node.label.as_ref())
        };
        for region in 0..snapshot.regions.len() {
            let scope = scope_of(region);
            snapshot.for_each_region_block(region, |slot, block| {
                if let Some(uid) = named(ids.blocks.get(slot)) {
                    candidates.push(Candidate {
                        uid,
                        label: format!("{scope}/{}", block.label),
                        level: Level::Block,
                    });
                    metadata.push(CandidateMeta { slot: slot as u32 });
                }
                snapshot.for_each_block_cell(slot, |cell, pod| {
                    if let Some(uid) = named(ids.cells.get(cell)) {
                        candidates.push(Candidate {
                            uid,
                            label: format!("{scope}/{}", pod.label),
                            level: Level::Cell,
                        });
                        metadata.push(CandidateMeta { slot: cell as u32 });
                    }
                });
                snapshot.for_each_block_sat(slot, |sat, node| {
                    if let Some(uid) = named(ids.sats.get(sat)) {
                        candidates.push(Candidate {
                            uid,
                            label: format!("{scope}/{}", node.label),
                            level: Level::Sat,
                        });
                        metadata.push(CandidateMeta { slot: sat as u32 });
                    }
                });
            });
        }
        debug_assert_eq!(candidates.len(), metadata.len());
        MapIndex {
            candidates,
            metadata,
        }
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Convenience for tests and one-shot callers. The interactive finder keeps
    /// its scratch allocation across keystrokes instead.
    pub fn rank(
        &self,
        snapshot: &SceneSnapshot,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>, String> {
        let query = CompiledQuery::compile(query)?;
        let mut scratch = RankScratch::default();
        Ok(scratch
            .rank(self, snapshot, &query, &OverlayIndex::default(), limit)
            .to_vec())
    }
}

#[derive(Debug, Clone, Copy)]
struct Ranked {
    candidate: usize,
    score: i64,
}

#[derive(Debug, Default)]
struct RankScratch {
    ranked: Vec<Ranked>,
    hits: Vec<Hit>,
}

impl RankScratch {
    fn rank<'a>(
        &'a mut self,
        index: &MapIndex,
        snapshot: &SceneSnapshot,
        query: &CompiledQuery,
        overlay: &OverlayIndex,
        limit: usize,
    ) -> &'a [Hit] {
        self.ranked.clear();
        if limit == 0 {
            return &[];
        }
        if self.ranked.capacity() < limit {
            self.ranked.reserve(limit);
        }
        if self.hits.len() < limit {
            self.hits.resize_with(limit, || Hit {
                candidate: 0,
                score: 0,
                hits: Vec::new(),
            });
        }
        let roots_fill_limit = query.is_unfiltered();

        for (candidate_index, candidate) in index.candidates.iter().enumerate() {
            if !query.matches(
                candidate,
                index.metadata[candidate_index],
                snapshot,
                overlay,
            ) {
                continue;
            }
            let Some(score) = fuzzy_score(&query.fuzzy, &candidate.label) else {
                continue;
            };
            let ranked = Ranked {
                candidate: candidate_index,
                score,
            };
            let at = self
                .ranked
                .partition_point(|current| rank_order(index, *current, ranked).is_lt());
            if at >= limit {
                continue;
            }
            if self.ranked.len() < limit {
                self.ranked.push(ranked);
                let end = self.ranked.len() - 1;
                self.ranked.copy_within(at..end, at + 1);
            } else {
                self.ranked.copy_within(at..limit - 1, at + 1);
            }
            self.ranked[at] = ranked;
            // Regions are indexed first and win every empty-query tie. Once
            // they fill the list, no later candidate can change the answer.
            if roots_fill_limit && self.ranked.len() == limit && candidate.level == Level::Region {
                break;
            }
        }

        for (output, ranked) in self.hits.iter_mut().zip(&self.ranked) {
            output.candidate = ranked.candidate;
            output.score = ranked.score;
            let candidate = &index.candidates[ranked.candidate];
            let matched = fuzzy_ranges_into(&query.fuzzy, &candidate.label, &mut output.hits);
            debug_assert!(matched, "a ranked candidate must also highlight");
        }
        &self.hits[..self.ranked.len()]
    }
}

fn rank_order(index: &MapIndex, left: Ranked, right: Ranked) -> Ordering {
    let left_candidate = &index.candidates[left.candidate];
    let right_candidate = &index.candidates[right.candidate];
    right
        .score
        .cmp(&left.score)
        .then((left_candidate.level as u8).cmp(&(right_candidate.level as u8)))
        .then(left.candidate.cmp(&right.candidate))
}

fn fuzzy_ranges_into(query: &str, candidate: &str, hits: &mut Vec<Range<usize>>) -> bool {
    hits.clear();
    let mut candidate = candidate.char_indices().map(|(start, character)| {
        (
            start..start + character.len_utf8(),
            character.to_lowercase().next().unwrap_or(character),
        )
    });
    for needle in query.chars().flat_map(char::to_lowercase) {
        if needle == ' ' {
            continue;
        }
        let Some((range, _)) = candidate.find(|(_, character)| *character == needle) else {
            return false;
        };
        match hits.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => hits.push(range),
        }
    }
    true
}

/// How many cluster hits a keystroke lists. Small because a list nobody can
/// read is not an answer, and highlighting is charged per survivor.
const CLUSTER_FINDER_LIMIT: usize = 32;

/// Structured and fuzzy search over a published scene. Object strings follow
/// identity revisions while status predicates read the latest health snapshot.
#[derive(Debug)]
pub struct ClusterFinder {
    index: MapIndex,
    snapshot: Arc<SceneSnapshot>,
    identity_rev: u64,
    scene_rev: u64,
    overlay: OverlayIndex,
    query: String,
    scratch: RankScratch,
    hit_count: usize,
    error: Option<String>,
    tracks_health: bool,
    tracks_overlay: bool,
    selected: usize,
}

impl ClusterFinder {
    pub fn open(snapshot: Arc<SceneSnapshot>) -> ClusterFinder {
        let mut finder = ClusterFinder {
            index: MapIndex::build(&snapshot),
            identity_rev: snapshot.identity_rev,
            scene_rev: snapshot.rev,
            snapshot,
            overlay: OverlayIndex::default(),
            query: String::new(),
            scratch: RankScratch::default(),
            hit_count: 0,
            error: None,
            tracks_health: false,
            tracks_overlay: false,
            selected: 0,
        };
        finder.requery();
        finder
    }

    /// Answers whether object identity changed and required an index rebuild.
    /// Health-only publishes still rerank structured status predicates against
    /// the current immutable snapshot.
    pub fn sync(&mut self, snapshot: Arc<SceneSnapshot>) -> bool {
        let identity_changed = snapshot.identity_rev != self.identity_rev;
        if !identity_changed && snapshot.rev == self.scene_rev {
            return false;
        }
        if identity_changed {
            self.index = MapIndex::build(&snapshot);
        }
        self.identity_rev = snapshot.identity_rev;
        self.scene_rev = snapshot.rev;
        self.snapshot = snapshot;
        if identity_changed || self.tracks_health || self.tracks_overlay {
            self.requery();
        }
        identity_changed
    }

    /// Overlay stamps land after first paint. A query that names one reranks
    /// when this table changes, the same way a status query reranks on health.
    pub fn set_overlay(&mut self, frame: OverlayFrame) {
        let next = OverlayIndex::from_frame(&frame);
        if next.kind == self.overlay.kind && next.uids == self.overlay.uids {
            return;
        }
        self.overlay = next;
        if self.tracks_overlay {
            self.requery();
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[Hit] {
        &self.scratch.hits[..self.hit_count]
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn index(&self) -> &MapIndex {
        &self.index
    }

    pub fn set_query(&mut self, query: String) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.requery();
    }

    pub fn push_char(&mut self, text: &str) {
        self.query.push_str(text);
        self.requery();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.requery();
    }

    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_down(&mut self) {
        if self.selected + 1 < self.hit_count {
            self.selected += 1;
        }
    }

    pub fn confirm(&self) -> Option<&Candidate> {
        let hit = self.hits().get(self.selected)?;
        self.index.candidates().get(hit.candidate)
    }

    fn requery(&mut self) {
        match CompiledQuery::compile(&self.query) {
            Ok(query) => {
                self.error = None;
                self.tracks_health = !query.statuses.is_empty();
                self.tracks_overlay = !query.overlays.is_empty();
                self.hit_count = self
                    .scratch
                    .rank(
                        &self.index,
                        &self.snapshot,
                        &query,
                        &self.overlay,
                        CLUSTER_FINDER_LIMIT,
                    )
                    .len();
            }
            Err(error) => {
                self.error = Some(error);
                self.tracks_health = false;
                self.tracks_overlay = false;
                self.hit_count = 0;
            }
        }
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{KindId, Rect, ToolId};
    use k10s_core::{NsExt, NsNode, PodExt, PodNode, Severity, State, WlExt, WorkloadNode};

    // (namespace, [(workload, [pod])]) keeps the fixture topology visible,
    // so a test reads as the scene it is about.
    type Workloads<'a> = &'a [(&'a str, &'a [&'a str])];
    type Namespaces<'a> = &'a [(&'a str, Workloads<'a>)];

    fn scene(namespaces: Namespaces<'_>) -> SceneSnapshot {
        let mut snap = SceneSnapshot::default();
        snap.identity_rev = 1;
        snap.rev = 1;
        let ids = Arc::make_mut(&mut snap.ids);
        let mut blocks = Vec::new();
        let mut cells = Vec::new();
        for (region, (ns, workloads)) in namespaces.iter().enumerate() {
            let first_block = blocks.len() as u32;
            for (workload, pods) in workloads.iter() {
                let first_cell = cells.len() as u32;
                for pod in pods.iter() {
                    cells.push(PodNode {
                        rect: Rect::new(0.0, 0.0, 8.0, 8.0),
                        label: Arc::from(*pod),
                        ext: PodExt { state: State::OK },
                    });
                    ids.cells
                        .push(Arc::from(format!("pod-{ns}-{workload}-{pod}")));
                }
                blocks.push(WorkloadNode {
                    rect: Rect::new(0.0, 0.0, 60.0, 40.0),
                    inner: Rect::new(2.0, 2.0, 56.0, 36.0),
                    label: Arc::from(*workload),
                    children: first_cell..cells.len() as u32,
                    sats: 0..0,
                    ext: WlExt {
                        kind: KindId::DEPLOYMENT,
                        tool: ToolId::NONE,
                        rollup: Severity::Ok,
                        ns: region as u32,
                    },
                });
                ids.blocks.push(Arc::from(format!("wl-{ns}-{workload}")));
            }
            snap.scene.regions.push(NsNode {
                rect: Rect::new(0.0, 0.0, 400.0, 300.0),
                label: Arc::from(*ns),
                weight: 0,
                children: first_block..blocks.len() as u32,
                ext: NsExt {
                    unhealthy_frac: 0.0,
                    rollup: Severity::Ok,
                },
            });
            ids.regions.push(Arc::from(format!("ns-{ns}")));
        }
        snap.scene.blocks = blocks;
        snap.scene.cells = cells;
        snap
    }

    fn labels(index: &MapIndex, hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|hit| index.candidates()[hit.candidate].label.clone())
            .collect()
    }

    #[test]
    fn every_level_is_searchable_and_a_pod_carries_its_namespace() {
        let snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        let index = MapIndex::build(&snapshot);
        let found: Vec<&str> = index
            .candidates()
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(found, ["prod", "prod/api", "prod/api-0", "prod/api-1"]);
        // Typing the namespace narrows without a separate filter, which is the
        // reason the name is not carried bare.
        assert_eq!(index.rank(&snapshot, "prod api-1", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_namespace_outranks_the_pods_named_after_it() {
        let snapshot = scene(&[("api", &[("api", &["api-0", "api-1", "api-2"])])]);
        let index = MapIndex::build(&snapshot);
        let hits = index.rank(&snapshot, "api", 10).unwrap();
        let ranked = labels(&index, &hits);
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("api"),
            "the namespace is the bigger answer and contains the rest: {ranked:?}"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_answers_nothing() {
        let snapshot = scene(&[("prod", &[("api", &["api-0"])])]);
        let index = MapIndex::build(&snapshot);
        assert!(index.rank(&snapshot, "zzzz", 10).unwrap().is_empty());
    }

    #[test]
    fn the_limit_is_honoured_and_zero_asks_for_no_work() {
        let pods: Vec<String> = (0..50).map(|i| format!("api-{i}")).collect();
        let borrowed: Vec<&str> = pods.iter().map(String::as_str).collect();
        let snapshot = scene(&[("prod", &[("api", &borrowed)])]);
        let index = MapIndex::build(&snapshot);
        assert_eq!(index.rank(&snapshot, "api", 8).unwrap().len(), 8);
        assert!(index.rank(&snapshot, "api", 0).unwrap().is_empty());
    }

    #[test]
    fn an_empty_query_offers_everything_in_scene_order() {
        let snapshot = scene(&[("prod", &[("api", &["api-0"])])]);
        let index = MapIndex::build(&snapshot);
        let hits = index.rank(&snapshot, "", 10).unwrap();
        let ranked = labels(&index, &hits);
        assert_eq!(ranked, ["prod", "prod/api", "prod/api-0"]);
    }

    #[test]
    fn a_tombstoned_slot_is_not_offered() {
        let mut snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        Arc::make_mut(&mut snapshot.ids).cells[0] = Arc::from("");
        let index = MapIndex::build(&snapshot);
        let hits = index.rank(&snapshot, "api-", 10).unwrap();
        let ranked = labels(&index, &hits);
        assert_eq!(
            ranked,
            ["prod/api-1"],
            "a hole in the scene was offered as somewhere to go"
        );
    }

    #[test]
    fn ranking_is_total_so_the_same_query_answers_the_same_way_twice() {
        let snapshot = scene(&[
            ("prod", &[("api", &["api-0", "api-1"])]),
            ("stage", &[("api", &["api-0"])]),
        ]);
        let index = MapIndex::build(&snapshot);
        let once = index.rank(&snapshot, "api", 10).unwrap();
        assert_eq!(once, index.rank(&snapshot, "api", 10).unwrap());
        // And every hit's ranges point into the label it was scored against.
        for hit in &once {
            let label = &index.candidates()[hit.candidate].label;
            for range in &hit.hits {
                assert!(
                    label.get(range.clone()).is_some(),
                    "a highlight range points outside {label:?}"
                );
            }
        }
    }

    #[test]
    fn bounded_top_k_is_deterministic_across_reused_scratch() {
        let pods: Vec<String> = (0..50).map(|index| format!("api-{index:02}")).collect();
        let borrowed: Vec<&str> = pods.iter().map(String::as_str).collect();
        let snapshot = Arc::new(scene(&[("prod", &[("api", &borrowed)])]));
        let mut finder = ClusterFinder::open(snapshot);
        let first: Vec<Arc<str>> = finder
            .hits()
            .iter()
            .map(|hit| finder.index().candidates()[hit.candidate].uid.clone())
            .collect();
        assert_eq!(first.len(), CLUSTER_FINDER_LIMIT);

        finder.push_char("z");
        finder.pop_char();
        let second: Vec<Arc<str>> = finder
            .hits()
            .iter()
            .map(|hit| finder.index().candidates()[hit.candidate].uid.clone())
            .collect();
        assert_eq!(second, first, "reusing the top-k storage changed tie order");
    }

    #[test]
    fn structured_predicates_are_anded_and_bare_reasons_are_exact() {
        let mut snapshot = scene(&[
            ("prod", &[("api", &["api-0", "api-1"])]),
            ("stage", &[("api", &["api-1"])]),
        ]);
        snapshot.cells[1].ext.state = State::of(ReasonId::CRASH_LOOP_BACK_OFF);
        let index = MapIndex::build(&snapshot);
        let query = "kind:Pod ns:prod status:crashloopbackoff uid:pod-prod-api-api-1 api-1";
        let hits = index.rank(&snapshot, query, 32).unwrap();
        assert_eq!(labels(&index, &hits), ["prod/api-1"]);

        let bare = index
            .rank(&snapshot, "kind:pod namespace:prod crashloopbackoff", 32)
            .unwrap();
        assert_eq!(labels(&index, &bare), ["prod/api-1"]);
        assert!(
            index
                .rank(&snapshot, "kind:pod ns:stage crashloopbackoff", 32)
                .unwrap()
                .is_empty(),
            "a reason predicate leaked across namespaces"
        );
    }

    #[test]
    fn unavailable_and_unknown_qualifiers_fail_instead_of_falling_back_to_fuzzy() {
        let snapshot = Arc::new(scene(&[("prod", &[("api", &["api-0"])])]));
        let mut finder = ClusterFinder::open(snapshot);
        for query in [
            "label:app=api",
            "overlay:grafana",
            "owner:api",
            "kind:",
            "kind:not-a-kubernetes-kind",
            "status:not-a-status",
        ] {
            finder.set_query(query.to_string());
            assert!(finder.error().is_some(), "{query:?} was silently accepted");
            assert!(finder.hits().is_empty(), "an invalid query returned hits");
            assert!(
                finder.confirm().is_none(),
                "an invalid query was confirmable"
            );
        }
    }

    #[test]
    fn a_health_publish_updates_status_without_rebuilding_identity_strings() {
        let snapshot = scene(&[("prod", &[("api", &["api-0"])])]);
        let mut finder = ClusterFinder::open(Arc::new(snapshot.clone()));
        finder.set_query("kind:pod status:crashloopbackoff".to_string());
        assert!(finder.hits().is_empty());
        let identities = finder.index().candidates().as_ptr();

        let mut changed = snapshot;
        changed.rev += 1;
        changed.cells[0].ext.state = State::of(ReasonId::CRASH_LOOP_BACK_OFF);
        assert!(!finder.sync(Arc::new(changed)));
        assert_eq!(finder.index().candidates().as_ptr(), identities);
        assert_eq!(
            finder.confirm().map(|candidate| candidate.uid.as_ref()),
            Some("pod-prod-api-api-0")
        );
    }

    #[test]
    fn overlay_qualifier_matches_stamped_uids_of_the_active_kind() {
        let snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        let mut finder = ClusterFinder::open(Arc::new(snapshot));
        finder.set_query("overlay:policy".to_string());
        assert!(
            finder.hits().is_empty(),
            "an overlay query with no stamps is not every object"
        );

        finder.set_overlay(OverlayFrame {
            kind: Some(OverlayKind::Policy),
            marks: vec![k10s_map::OverlayMark {
                uid: "pod-prod-api-api-1".into(),
                tint: None,
                sparkline: None,
                label: None,
            }],
        });
        assert_eq!(
            finder.confirm().map(|candidate| candidate.uid.as_ref()),
            Some("pod-prod-api-api-1")
        );

        finder.set_query("overlay:metrics".to_string());
        assert!(
            finder.hits().is_empty(),
            "a metrics query must not reuse policy stamps"
        );
        finder.set_query("overlay:grafana".to_string());
        assert!(
            finder
                .error()
                .is_some_and(|error| error.contains("unknown overlay")),
            "{:?}",
            finder.error()
        );
    }

    #[test]
    fn confirmation_keeps_the_published_uid_across_health_updates() {
        let snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        let mut finder = ClusterFinder::open(Arc::new(snapshot.clone()));
        finder.set_query("uid:pod-prod-api-api-1".to_string());
        let uid = finder.confirm().unwrap().uid.clone();

        let mut changed = snapshot;
        changed.rev += 1;
        changed.cells[1].ext.state = State::of(ReasonId::OOM_KILLED);
        finder.sync(Arc::new(changed));
        assert_eq!(finder.confirm().map(|candidate| &candidate.uid), Some(&uid));
    }

    #[test]
    fn confirm_hands_back_a_uid_the_snapshot_can_still_locate() {
        let snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        let mut finder = ClusterFinder::open(Arc::new(snapshot.clone()));
        finder.set_query("uid:pod-prod-api-api-1".to_string());
        let uid = finder
            .confirm()
            .expect("uid query should confirm")
            .uid
            .clone();
        let located = snapshot
            .locate(uid.as_ref())
            .expect("the published uid must still be on the scene the map flies to");
        assert_eq!(located.level, Level::Cell);
        assert_eq!(uid.as_ref(), "pod-prod-api-api-1");
    }

    #[test]
    fn the_cluster_finder_ranks_without_rebuilding_the_index() {
        let mut snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        let mut finder = ClusterFinder::open(Arc::new(snapshot.clone()));
        let first = finder.index().candidates().as_ptr();
        assert!(
            !finder.sync(Arc::new(snapshot.clone())),
            "the same revision must not rebuild"
        );
        finder.push_char("api");
        assert_eq!(
            finder.index().candidates().as_ptr(),
            first,
            "a keystroke must not rebuild the index"
        );
        let ranked = labels(finder.index(), finder.hits());
        assert!(
            ranked.iter().any(|label| label == "prod/api"),
            "ranking still answers from the index built at open: {ranked:?}"
        );

        snapshot.rev = 2;
        assert!(
            !finder.sync(Arc::new(snapshot.clone())),
            "a health-only revision must reuse the identity index"
        );
        assert_eq!(finder.index().candidates().as_ptr(), first);
        finder.set_query("api-1".to_string());
        assert!(
            !finder.sync(Arc::new(snapshot.clone())),
            "a second keystroke on the same revision must not rebuild again"
        );
        assert_eq!(
            finder.confirm().map(|candidate| candidate.label.as_str()),
            Some("prod/api-1")
        );

        snapshot.identity_rev = 2;
        snapshot.rev = 3;
        assert!(
            finder.sync(Arc::new(snapshot)),
            "new object identity rebuilds exactly once"
        );
    }
}
