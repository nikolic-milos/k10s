//! Highlight identity survives revisions but never slot reuse.

use k10s_core::SceneSnapshot;

use crate::{PickPath, path_rect};

// A highlight is an index path plus the identities which made that path true.
// World slots are intentionally reused; validating the non-empty ids before a
// paint prevents a deleted pod's ring from jumping to its replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mark {
    path: PickPath,
    rev: u64,
    ids: [Option<std::sync::Arc<str>>; 4],
}

impl Mark {
    pub(super) fn new(scene: &SceneSnapshot, path: PickPath) -> Option<Mark> {
        path_rect(scene, &path)?;
        let id = |ids: &k10s_core::SlotIds, slot: Option<u32>| {
            slot.and_then(|slot| ids.get(slot as usize))
                .filter(|id| !id.is_empty())
                .cloned()
        };
        Some(Mark {
            path,
            rev: scene.rev,
            ids: [
                id(&scene.ids.regions, Some(path.region)),
                id(&scene.ids.blocks, path.block),
                id(&scene.ids.cells, path.cell),
                id(&scene.ids.sats, path.sat),
            ],
        })
    }

    pub(super) fn resolve(&self, scene: &SceneSnapshot) -> Option<PickPath> {
        path_rect(scene, &self.path)?;
        let current = [
            scene.ids.regions.get(self.path.region as usize),
            self.path
                .block
                .and_then(|slot| scene.ids.blocks.get(slot as usize)),
            self.path
                .cell
                .and_then(|slot| scene.ids.cells.get(slot as usize)),
            self.path
                .sat
                .and_then(|slot| scene.ids.sats.get(slot as usize)),
        ];
        let mut identified = false;
        for (expected, current) in self.ids.iter().zip(current) {
            if let Some(expected) = expected {
                identified = true;
                if current != Some(expected) {
                    return None;
                }
            }
        }
        (identified || scene.rev == self.rev).then_some(self.path)
    }
}
