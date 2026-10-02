//! One-shot presentation milestones for startup measurement.

use std::time::Instant;

use gpui::{App, Window};
use k10s_core::SceneSnapshot;

type PresentCallback = Box<dyn FnOnce(Instant, &mut App)>;
type SceneReady = Box<dyn Fn(&SceneSnapshot) -> bool>;

/// One-shot observations of frames that crossed GPUI's presentation boundary.
///
/// GPUI does not expose the platform renderer's submit callback. A callback
/// armed during a draw therefore runs at the beginning of the next platform
/// frame: after the frame that armed it was submitted, and before any later
/// frame is built. Keeping this seam here lets the application measure startup
/// without putting clocks, serialization, or process policy in the painter.
#[derive(Default)]
pub struct PresentProbe {
    first: Option<PresentCallback>,
    scene: Option<PresentCallback>,
    scene_ready: Option<SceneReady>,
}

impl PresentProbe {
    pub fn first(callback: impl FnOnce(Instant, &mut App) + 'static) -> Self {
        Self {
            first: Some(Box::new(callback)),
            scene: None,
            scene_ready: None,
        }
    }

    pub fn on_scene(mut self, callback: impl FnOnce(Instant, &mut App) + 'static) -> Self {
        self.scene_ready = Some(Box::new(|scene| scene.rev != 0));
        self.scene = Some(Box::new(callback));
        self
    }

    pub fn on_scene_when(
        mut self,
        ready: impl Fn(&SceneSnapshot) -> bool + 'static,
        callback: impl FnOnce(Instant, &mut App) + 'static,
    ) -> Self {
        self.scene_ready = Some(Box::new(ready));
        self.scene = Some(Box::new(callback));
        self
    }

    fn take(&mut self, scene_ready: bool) -> ArmedPresent {
        if scene_ready {
            self.scene_ready = None;
        }
        ArmedPresent {
            first: self.first.take(),
            scene: scene_ready.then(|| self.scene.take()).flatten(),
        }
    }

    pub(super) fn arm(&mut self, scene: &SceneSnapshot, window: &Window) {
        let scene_ready = self.scene_ready.as_ref().is_some_and(|ready| ready(scene));
        let callbacks = self.take(scene_ready);
        if callbacks.is_empty() {
            return;
        }
        window.on_next_frame(move |_, cx| callbacks.fire(Instant::now(), cx));
    }
}

struct ArmedPresent {
    first: Option<PresentCallback>,
    scene: Option<PresentCallback>,
}

impl ArmedPresent {
    fn is_empty(&self) -> bool {
        self.first.is_none() && self.scene.is_none()
    }

    fn fire(self, presented_at: Instant, cx: &mut App) {
        if let Some(callback) = self.first {
            callback(presented_at, cx);
        }
        if let Some(callback) = self.scene {
            callback(presented_at, cx);
        }
    }
}

#[cfg(test)]
#[path = "present_test.rs"]
mod present_tests;
