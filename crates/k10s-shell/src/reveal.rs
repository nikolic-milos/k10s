//! Finding a thing on the map by typing part of its name.
//!
//! Two halves with very different costs, kept apart for that reason. Building
//! the searchable list walks every object in the cluster and allocates a string
//! per row, so it happens once per published revision. Ranking runs on every
//! keystroke over a list that is already built, so it allocates nothing per
//! candidate and stops at a bounded number of answers.
//!
//! That split is the whole design. A fifty-thousand-object cluster has fifty
//! thousand candidates, a person types into a search box at ten characters a
//! second, and a keystroke has one 60 Hz frame to answer in. Rebuilding the list
//! per keystroke would spend that frame on work whose input did not change.

use std::sync::Arc;

use k10s_core::{Level, SceneSnapshot};

use crate::palette::{fuzzy_match, fuzzy_score};

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

/// One hit, with where the query matched so the row can show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub candidate: usize,
    pub score: i64,
    pub hits: Vec<std::ops::Range<usize>>,
}

/// Everything on the map that can be searched, built once per published scene.
#[derive(Debug, Default)]
pub struct MapIndex {
    candidates: Vec<Candidate>,
}

impl MapIndex {
    /// Walk a published scene into a searchable list.
    ///
    /// Three milliseconds at fifty thousand objects, which is why it carries no
    /// frame budget and must not be run per publish: the world publishes at
    /// 20 Hz and would spend six percent of every frame rebuilding a list nobody
    /// has opened. Build it when the search opens, and again when the scene
    /// revision it was built from is no longer the one on screen.
    ///
    /// Tombstoned slots hold the empty uid and are skipped: a hole in the scene
    /// is not an object, and a row that reveals nothing is worse than no row.
    /// Regions come first and satellites last so that equal scores fall out in a
    /// useful order without a second sort key doing the work -- see `rank`.
    pub fn build(snapshot: &SceneSnapshot) -> MapIndex {
        let ids = &snapshot.ids;
        let mut candidates = Vec::new();
        let named = |uid: Option<&Arc<str>>| uid.filter(|uid| !uid.is_empty()).cloned();

        for (slot, node) in snapshot.regions.iter().enumerate() {
            if let Some(uid) = named(ids.regions.get(slot)) {
                candidates.push(Candidate {
                    uid,
                    label: node.label.to_string(),
                    level: Level::Region,
                });
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
                }
                snapshot.for_each_block_cell(slot, |cell, pod| {
                    if let Some(uid) = named(ids.cells.get(cell)) {
                        candidates.push(Candidate {
                            uid,
                            label: format!("{scope}/{}", pod.label),
                            level: Level::Cell,
                        });
                    }
                });
                snapshot.for_each_block_sat(slot, |sat, node| {
                    if let Some(uid) = named(ids.sats.get(sat)) {
                        candidates.push(Candidate {
                            uid,
                            label: format!("{scope}/{}", node.label),
                            level: Level::Sat,
                        });
                    }
                });
            });
        }
        MapIndex { candidates }
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// The best `limit` matches for a query, best first.
    ///
    /// Scored in one pass that allocates nothing, then highlighted only for the
    /// rows that survived. `fuzzy_match` builds a character vector and a range
    /// vector per candidate it looks at, and at fifty thousand candidates that
    /// is a hundred thousand allocations per keystroke for ranges that belong to
    /// rows nobody will see -- the `shell state` bench measured it at 5.50 ms
    /// against a 16.67 ms frame, a third of a frame spent on a scan whose answer
    /// is thirty-two rows. Highlighting the survivors costs `limit` matches, and
    /// the limit is small because a list nobody can read is not an answer.
    /// Measured, the split took that keystroke from 5.50 ms to 1.57 ms.
    ///
    /// Ties break towards the *shallower* object. A cluster with a namespace
    /// called `api` and forty pods called `api-something` should offer the
    /// namespace first: it is the bigger answer, and it contains the others, so
    /// a person who wanted a pod is one keystroke away while a person who wanted
    /// the namespace is already there. Equal level and equal score break by
    /// position, so the order is total and a rank is reproducible.
    pub fn rank(&self, query: &str, limit: usize) -> Vec<Hit> {
        let mut found: Vec<Hit> = Vec::new();
        if limit == 0 {
            return found;
        }
        for (index, candidate) in self.candidates.iter().enumerate() {
            let Some(score) = fuzzy_score(query, &candidate.label) else {
                continue;
            };
            found.push(Hit {
                candidate: index,
                score,
                hits: Vec::new(),
            });
        }
        found.sort_by(|a, b| {
            let (left, right) = (&self.candidates[a.candidate], &self.candidates[b.candidate]);
            b.score
                .cmp(&a.score)
                .then((left.level as u8).cmp(&(right.level as u8)))
                .then(a.candidate.cmp(&b.candidate))
        });
        found.truncate(limit);
        for hit in &mut found {
            if let Some((_, hits)) = fuzzy_match(query, &self.candidates[hit.candidate].label) {
                hit.hits = hits;
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{KindId, Rect, ToolId};
    use k10s_core::{NsExt, NsNode, PodExt, PodNode, Severity, State, WlExt, WorkloadNode};

    // (namespace, [(workload, [pod])]) -- a whole cluster shape as one literal,
    // so a test reads as the scene it is about.
    type Workloads<'a> = &'a [(&'a str, &'a [&'a str])];
    type Namespaces<'a> = &'a [(&'a str, Workloads<'a>)];

    fn scene(namespaces: Namespaces<'_>) -> SceneSnapshot {
        let mut snap = SceneSnapshot::default();
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
                    ids.cells.push(Arc::from(format!("pod-{pod}")));
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
                ids.blocks.push(Arc::from(format!("wl-{workload}")));
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
        let index = MapIndex::build(&scene(&[("prod", &[("api", &["api-0", "api-1"])])]));
        let found: Vec<&str> = index
            .candidates()
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(found, ["prod", "prod/api", "prod/api-0", "prod/api-1"]);
        // Typing the namespace narrows without a separate filter, which is the
        // reason the name is not carried bare.
        assert_eq!(index.rank("prod api-1", 10).len(), 1);
    }

    #[test]
    fn a_namespace_outranks_the_pods_named_after_it() {
        let index = MapIndex::build(&scene(&[("api", &[("api", &["api-0", "api-1", "api-2"])])]));
        let ranked = labels(&index, &index.rank("api", 10));
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("api"),
            "the namespace is the bigger answer and contains the rest: {ranked:?}"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_answers_nothing() {
        let index = MapIndex::build(&scene(&[("prod", &[("api", &["api-0"])])]));
        assert!(index.rank("zzzz", 10).is_empty());
    }

    #[test]
    fn the_limit_is_honoured_and_zero_asks_for_no_work() {
        let pods: Vec<String> = (0..50).map(|i| format!("api-{i}")).collect();
        let borrowed: Vec<&str> = pods.iter().map(String::as_str).collect();
        let index = MapIndex::build(&scene(&[("prod", &[("api", &borrowed)])]));
        assert_eq!(index.rank("api", 8).len(), 8);
        assert!(index.rank("api", 0).is_empty());
    }

    #[test]
    fn an_empty_query_offers_everything_in_scene_order() {
        let index = MapIndex::build(&scene(&[("prod", &[("api", &["api-0"])])]));
        let ranked = labels(&index, &index.rank("", 10));
        assert_eq!(ranked, ["prod", "prod/api", "prod/api-0"]);
    }

    #[test]
    fn a_tombstoned_slot_is_not_offered() {
        let mut snapshot = scene(&[("prod", &[("api", &["api-0", "api-1"])])]);
        Arc::make_mut(&mut snapshot.ids).cells[0] = Arc::from("");
        let index = MapIndex::build(&snapshot);
        let ranked = labels(&index, &index.rank("api-", 10));
        assert_eq!(
            ranked,
            ["prod/api-1"],
            "a hole in the scene was offered as somewhere to go"
        );
    }

    #[test]
    fn ranking_is_total_so_the_same_query_answers_the_same_way_twice() {
        let index = MapIndex::build(&scene(&[
            ("prod", &[("api", &["api-0", "api-1"])]),
            ("stage", &[("api", &["api-0"])]),
        ]));
        let once = index.rank("api", 10);
        assert_eq!(once, index.rank("api", 10));
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
}
