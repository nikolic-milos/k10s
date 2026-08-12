//! Choosing a cluster, connecting to one, and what the window does when it
//! arrives.
//!
//! Nothing is given up until the new connection exists. A refused attempt has to
//! leave the window exactly as it was -- somebody who mistypes a context while
//! looking at production must not lose production for it -- so the chooser stays
//! open and usable on failure, and the seam retires the previous cluster only
//! once the next one has synced. When one does arrive, adopting is a provider
//! swap and a notify, because every view reads through the one slot rather than
//! through a clone it was built with; what that cannot fix is a tab whose
//! *content* came out of the cluster that just left, which is what
//! [`Workspace::retire_cluster_views`] is for.

use gpui::{AppContext as _, Context, Entity, Window};

use crate::finder::PickerMode;
use crate::launch::{self, LaunchEvent, LaunchView};
use crate::provider::{ConnectOutcome, ConnectRequest, Connection, DemoOutcome, ScanRequest};
use crate::workspace::{PickerPurpose, Workspace};

impl Workspace {
    /// Land the connection explicitly requested by the command line.
    ///
    /// Unlike a chooser attempt, this has no previous scene to preserve and no
    /// open chooser in which to recover. The empty workspace appears while the
    /// data plane syncs; a refusal retains the command-line contract by ending
    /// the application with the failure already reported on stderr.
    pub fn await_command_line_connection(
        &mut self,
        reply: futures::channel::oneshot::Receiver<ConnectOutcome>,
        on_failure: impl FnOnce() + 'static,
        cx: &mut Context<Self>,
    ) {
        self.scene_chosen = true;
        self.status_note = Some("connecting to the cluster".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = reply.await;
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(ConnectOutcome::Connected(connection)) => this.adopt(connection, cx),
                Ok(ConnectOutcome::Failed(why)) => {
                    on_failure();
                    eprintln!("k10s: {why}");
                    this.status_note = Some(why);
                    cx.notify();
                    cx.quit();
                }
                Err(_) => {
                    on_failure();
                    let why = "the connection attempt was dropped".to_string();
                    eprintln!("k10s: {why}");
                    this.status_note = Some(why);
                    cx.notify();
                    cx.quit();
                }
            });
        })
        .detach();
    }

    /// Land a generated scene that started before the native window.
    ///
    /// CPU work begins in the application crate so it overlaps GPUI renderer
    /// creation. Its answer belongs here: loading status and the transition
    /// from an empty map to a chosen scene are shell state.
    pub fn await_generated_scene(
        &mut self,
        reply: futures::channel::oneshot::Receiver<DemoOutcome>,
        cx: &mut Context<Self>,
    ) {
        self.scene_chosen = true;
        self.status_note = Some("building the starmap".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = reply.await;
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(DemoOutcome::Started(summary)) => {
                    this.chose_scene(cx);
                    this.status_note = Some(summary);
                    cx.notify();
                }
                Ok(DemoOutcome::Failed(why)) => {
                    this.scene_chosen = false;
                    this.status_note = Some(why);
                    cx.notify();
                }
                Err(_) => {
                    this.scene_chosen = false;
                    this.status_note = Some("the generator was dropped".to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Show the chooser: the contexts this process can see, a way to reach a
    /// kubeconfig it cannot, and the generated starmap.
    ///
    /// Opened at startup when the command line named no cluster, and reopenable
    /// from the palette or its chord at any time after. It is an overlay rather
    /// than a separate window because the workspace behind it is already the
    /// thing being filled in.
    pub fn open_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.launch.is_open() {
            return;
        }
        self.close_palette(window, cx);
        let view = cx.new(LaunchView::new);
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, view, event: &LaunchEvent, window, cx| match event {
                LaunchEvent::Dismissed => this.dismiss_launch(window, cx),
                LaunchEvent::Chose(choice) => {
                    this.chose_launch(view.clone(), choice.clone(), window, cx)
                }
            },
        );
        self.launch.open(view.clone(), subscription, window, cx);
        cx.notify();
        self.scan_launch(&view, ScanRequest::Detected, cx);
    }

    pub(crate) fn toggle_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch.is_open() {
            self.dismiss_launch(window, cx);
        } else {
            self.open_launch(window, cx);
        }
    }

    pub(crate) fn close_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch.close(window, cx) {
            cx.notify();
        }
    }

    // Escape, or a click outside. Leaving with nothing chosen is allowed -- an
    // empty map is a legitimate place to stand -- but it has to say how to come
    // back, because the alternative is an empty window and a guess.
    pub(crate) fn dismiss_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nothing_chosen = !self.scene_chosen;
        self.close_launch(window, cx);
        if nothing_chosen {
            self.status_note =
                Some("nothing chosen; ctrl-k ctrl-c picks a cluster or the starmap".to_string());
            cx.notify();
        }
    }

    // Reading and merging kubeconfigs is file I/O on paths that can be stalled
    // network mounts, so it happens behind the seam and lands here one turn
    // later. The request travels with the answer: two scans can be in flight and
    // each has to land under its own header.
    pub(crate) fn scan_launch(
        &mut self,
        view: &Entity<LaunchView>,
        request: ScanRequest,
        cx: &mut Context<Self>,
    ) {
        view.update(cx, |view, cx| view.rescanning(cx));
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.scan(
            request.clone(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Ok(outcome) = rx.await {
                let _ = view.update(cx, |view, cx| view.scanned(&request, outcome, cx));
            }
        })
        .detach();
    }

    pub(crate) fn scan_kubeconfig(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let Some(view) = self.launch.view().cloned() else {
            return;
        };
        self.scan_launch(&view, ScanRequest::File(path), cx);
    }

    pub(crate) fn chose_launch(
        &mut self,
        view: Entity<LaunchView>,
        choice: launch::Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match choice {
            launch::Choice::OpenKubeconfig => {
                // The picker opens over the chooser rather than instead of it,
                // so dismissing it returns to the list already on screen.
                let seed = kubeconfig_seed();
                self.open_picker(
                    seed,
                    PickerMode::OpenFile,
                    PickerPurpose::Kubeconfig,
                    window,
                    cx,
                );
            }
            launch::Choice::Demo => self.start_demo(view, window, cx),
            launch::Choice::Context { request, .. } => self.connect(view, request, window, cx),
        }
    }

    pub(crate) fn connect(
        &mut self,
        view: Entity<LaunchView>,
        request: ConnectRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing is given up until the new connection exists. A refused attempt
        // has to leave the window exactly as it was -- somebody who mistypes a
        // context while looking at production must not lose production for it --
        // and the seam is written the same way: it retires the previous cluster
        // only once the next one has synced.
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.connect(
            request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        let this = cx.weak_entity();
        let view = view.downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(outcome) = rx.await else {
                    let _ = view.update(cx, |view, cx| {
                        view.refused("the connection attempt was dropped".to_string(), cx)
                    });
                    return;
                };
                match outcome {
                    ConnectOutcome::Connected(connection) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.adopt(connection, cx);
                            this.close_launch(window, cx);
                        });
                    }
                    // The screen stays open and usable, which is the whole point
                    // of it being a screen: an unreachable cluster is where a
                    // dead end would cost the most.
                    ConnectOutcome::Failed(why) => {
                        let _ = view.update(cx, |view, cx| view.refused(why, cx));
                    }
                }
            })
            .detach();
    }

    // Adopting is a provider swap and a notify, because every view reads through
    // the one slot rather than through a clone it was built with.
    pub(crate) fn adopt(&mut self, connection: Connection, cx: &mut Context<Self>) {
        // Every cluster-shaped view open right now belongs to the connection this
        // one replaces, and a table that keeps painting a cluster the window has
        // left is the one failure nothing on screen would admit to.
        let retired = self.retire_cluster_views(cx);
        self.slot.set((connection.provider)());
        self.connected = true;
        self.chose_scene(cx);
        self.context = connection.context;
        self.status_note = Some(adopt_note(
            connection.summary,
            connection.notes.len(),
            retired,
        ));
        self.refresh_detail(cx);
        cx.notify();
    }

    // A scene has been chosen, whatever it is. The map forgets its framing rather
    // than being told to fit: the scene this is about is still on its way, and the
    // camera that framed the last one says nothing about it.
    pub(crate) fn chose_scene(&mut self, cx: &mut Context<Self>) {
        self.scene_chosen = true;
        self.map.update(cx, |map, cx| map.refit(cx));
    }

    pub(crate) fn start_demo(
        &mut self,
        view: Entity<LaunchView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.generate(Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }));
        let this = cx.weak_entity();
        let weak_view = view.downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(outcome) = rx.await else {
                    let _ = weak_view.update(cx, |view, cx| {
                        view.refused("the generator was dropped".to_string(), cx)
                    });
                    return;
                };
                match outcome {
                    DemoOutcome::Started(summary) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.chose_scene(cx);
                            this.status_note = Some(summary);
                            this.close_launch(window, cx);
                        });
                    }
                    DemoOutcome::Failed(why) => {
                        let _ = weak_view.update(cx, |view, cx| view.refused(why, cx));
                    }
                }
            })
            .detach();
    }

    // Which tabs a cluster switch invalidates is [`ItemTag::on_adopt`], asked
    // once per collection here. A fourth collection is a fourth line, and that is
    // the remaining hand-written thing in this function.
    pub(crate) fn retire_cluster_views(&mut self, cx: &mut Context<Self>) -> usize {
        let held = self.center.active().map(|tab| tab.tag.clone());
        let before = self.center.len() + self.left.len() + self.bottom.len();
        // The map never retires, so the center can never empty here.
        self.center.retain(|tab| !tab.tag.retires_on_adopt());
        self.left.retain(|tab| !tab.tag.retires_on_adopt());
        self.bottom.retain(|tab| !tab.tag.retires_on_adopt());
        // The tab strip is corrected without `activate_center`, because that
        // focuses what it activates -- and the chooser is still on screen and
        // still the thing the keyboard belongs to. Taking focus here left a
        // refused connection with a list the arrow keys could no longer move.
        //
        // A tab that left with the cluster hands the row back to index zero,
        // which is the map, rather than to whichever survivor inherited its
        // place: the switch is about looking at the new cluster, and the map is
        // the one thing already showing it. That is why the index is chosen here
        // instead of being left to `Pane::retain`.
        let restored = held
            .and_then(|tag| self.center.find(|tab| tab.tag == tag))
            .unwrap_or(0);
        self.center.activate(restored);
        cx.notify();
        before - (self.center.len() + self.left.len() + self.bottom.len())
    }
}

// Where a kubeconfig usually is, which is the only useful guess: the picker
// lists whatever is there and a typed path overrides it.
pub(crate) fn kubeconfig_seed() -> std::path::PathBuf {
    let kube = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| std::path::PathBuf::from(home).join(".kube"));
    crate::workspace::seed_dir(kube.as_deref())
}

// The one sentence an adopted connection shows. The notes themselves go to
// stderr, where they always went; their count goes here, because somebody who
// launched from a desktop entry has no stderr to look at and must at least
// know there is something to look for. A value rather than a method on the
// workspace so the sentence can be checked without a window.
pub(crate) fn adopt_note(summary: String, notes: usize, retired: usize) -> String {
    let mut note = summary;
    match notes {
        0 => {}
        1 => note.push_str("  ·  1 degradation note on stderr"),
        many => note.push_str(&format!("  ·  {many} degradation notes on stderr")),
    }
    if retired > 0 {
        note.push_str(&format!(
            "  ·  closed {retired} view{} belonging to the previous cluster",
            if retired == 1 { "" } else { "s" }
        ));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::adopt_note;

    #[test]
    fn the_adopt_note_counts_what_stderr_holds_and_what_the_switch_closed() {
        let plain = adopt_note("connected to prd".to_string(), 0, 0);
        assert_eq!(plain, "connected to prd");

        let one = adopt_note("connected to prd".to_string(), 1, 0);
        assert_eq!(one, "connected to prd  ·  1 degradation note on stderr");

        let many = adopt_note("connected to prd".to_string(), 3, 1);
        assert_eq!(
            many,
            "connected to prd  ·  3 degradation notes on stderr  ·  closed 1 view \
             belonging to the previous cluster"
        );

        let plural = adopt_note("connected to prd".to_string(), 0, 2);
        assert_eq!(
            plural,
            "connected to prd  ·  closed 2 views belonging to the previous cluster"
        );
    }
}
