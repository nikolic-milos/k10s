//! The app shell: the workspace that hosts the Starmap and everything docked
//! around it.
//!
//! The shell owns selection, actions, items, and panels; the map stays a view
//! that paints snapshots and emits picks. State crosses this boundary as
//! values -- a `Picked` carries the exact snapshot the user clicked on, and a
//! `Selection` is derived from it by a pure function, so a panel can never
//! disagree with the frame that was on screen. The center is a row of items
//! -- the map, the kind browser, the node capacity table, describe documents,
//! live log follows -- switched by tabs and keyed actions; every read goes
//! through the [`ReadProvider`] seam, so the shell never sees kube, and every
//! denial arrives as a labelled state. Keybindings are scoped by context
//! (`Workspace`, `Browse`, `Doc`, `Typing`), with the plain-letter commands
//! suppressed while an input mode is capturing text. Panels and items render
//! on notify only: zero paints at idle is a gated invariant and the shell
//! must never be the reason it fails.

pub mod browse;
pub mod provider;
pub mod table;
pub mod text;

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, FocusHandle, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    NoAction, ParentElement, Render, SharedString, Styled, Subscription, Window, actions, div,
    prelude::*, px, rgb,
};

use k10s_core::{KindId, Level, SceneSnapshot, kind_short};
use k10s_map::{MapView, PickPath, Picked};

use browse::{BrowseEvent, BrowseView};
pub use provider::{
    ContainersOutcome, DescribeRequest, Detail, DocOutcome, EventRow, KindRow, LogChunk,
    LogRequest, LogStop, NullProvider, ReadProvider, Reply, TableColumn, TableOutcome, TablePage,
    TableRow,
};
use text::TextView;

actions!(
    k10s_shell,
    [
        ToggleInspector,
        ClearSelection,
        OpenBrowser,
        OpenNodes,
        DescribeSelection,
        LogsSelection,
        NextItem,
        PrevItem,
        CloseItem,
        RowUp,
        RowDown,
        RowPageUp,
        RowPageDown,
        RowHome,
        RowEnd,
        OpenRow,
        LogsRow,
        Refresh,
        EnterFilter,
        Back,
        DocScrollUp,
        DocScrollDown,
        DocPageUp,
        DocPageDown,
        DocHome,
        DocEnd,
        EnterSearch,
        NextMatch,
        PrevMatch,
        ToggleFollow,
        ToggleTimestamps,
        CycleContainer,
        TogglePrevious,
        Reload,
        CancelDoc,
        CommitInput,
        CancelInput,
        DeleteInputChar,
    ]
);

pub fn keybindings() -> Vec<KeyBinding> {
    let workspace = Some("Workspace");
    let browse = Some("Browse");
    let doc = Some("Doc");
    let typing = Some("Typing");
    let mut bindings = vec![
        KeyBinding::new("i", ToggleInspector, workspace),
        KeyBinding::new("escape", ClearSelection, workspace),
        KeyBinding::new("b", OpenBrowser, workspace),
        KeyBinding::new("n", OpenNodes, workspace),
        KeyBinding::new("d", DescribeSelection, workspace),
        KeyBinding::new("l", LogsSelection, workspace),
        KeyBinding::new("ctrl-tab", NextItem, workspace),
        KeyBinding::new("ctrl-shift-tab", PrevItem, workspace),
        KeyBinding::new("ctrl-w", CloseItem, workspace),
        KeyBinding::new("up", RowUp, browse),
        KeyBinding::new("down", RowDown, browse),
        KeyBinding::new("pageup", RowPageUp, browse),
        KeyBinding::new("pagedown", RowPageDown, browse),
        KeyBinding::new("home", RowHome, browse),
        KeyBinding::new("end", RowEnd, browse),
        KeyBinding::new("enter", OpenRow, browse),
        KeyBinding::new("l", LogsRow, browse),
        KeyBinding::new("r", Refresh, browse),
        KeyBinding::new("/", EnterFilter, browse),
        KeyBinding::new("escape", Back, browse),
        KeyBinding::new("up", DocScrollUp, doc),
        KeyBinding::new("down", DocScrollDown, doc),
        KeyBinding::new("pageup", DocPageUp, doc),
        KeyBinding::new("pagedown", DocPageDown, doc),
        KeyBinding::new("home", DocHome, doc),
        KeyBinding::new("end", DocEnd, doc),
        KeyBinding::new("/", EnterSearch, doc),
        KeyBinding::new("n", NextMatch, doc),
        KeyBinding::new("shift-n", PrevMatch, doc),
        KeyBinding::new("f", ToggleFollow, doc),
        KeyBinding::new("t", ToggleTimestamps, doc),
        KeyBinding::new("c", CycleContainer, doc),
        KeyBinding::new("p", TogglePrevious, doc),
        KeyBinding::new("r", Reload, doc),
        KeyBinding::new("escape", CancelDoc, doc),
        KeyBinding::new("enter", CommitInput, typing),
        KeyBinding::new("escape", CancelInput, typing),
        KeyBinding::new("backspace", DeleteInputChar, typing),
    ];
    // While an input mode is capturing text, the workspace's plain-letter
    // commands must type, not dispatch: a NoAction binding in the deeper
    // context stops the ancestor match, and the character falls through to
    // the view's key handler.
    for key in ["i", "b", "n", "d", "l"] {
        bindings.push(KeyBinding::new(key, NoAction, typing));
    }
    bindings
}

// What the user has selected, named well enough to ask the cluster about it:
// the uid keys identity across publishes, the kind id names the API resource
// a describe needs, the labels are for people, and the ancestry is what a
// data-plane request needs (a pod's logs want its namespace's name, not its
// slot).
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
        let id_of = |ids: &[Arc<str>], slot: u32| {
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
}

const PANEL_BG: u32 = 0x14121f;
const PANEL_BORDER: u32 = 0x2c2842;
const PANEL_HEADING: u32 = 0xb8b2d9;
const PANEL_LABEL: u32 = 0x6e6890;
const PANEL_VALUE: u32 = 0xcfcae6;
const TAB_BAR_BG: u32 = 0x0b0a12;
const TAB_ACTIVE_BG: u32 = 0x2c2842;
const TAB_TEXT: u32 = 0xb8b2d9;
const TAB_DIM: u32 = 0x6e6890;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemTag {
    Map,
    Browse,
    Nodes,
    Doc(String),
    Logs(String),
}

enum ItemView {
    Map,
    Browse(Entity<BrowseView>, #[allow(dead_code)] Subscription),
    Text(Entity<TextView>),
}

struct Item {
    tag: ItemTag,
    view: ItemView,
}

pub struct Workspace {
    map: Entity<MapView>,
    items: Vec<Item>,
    active: usize,
    selection: Option<Selection>,
    inspector_open: bool,
    bench: bool,
    provider: Rc<dyn ReadProvider>,
    connected: bool,
    events: Option<Detail>,
    log: Option<Detail>,
    fetch_generation: u64,
    _pick_subscription: Subscription,
}

impl Workspace {
    pub fn new(
        map: Entity<MapView>,
        bench: bool,
        provider: Option<Rc<dyn ReadProvider>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pick_subscription = cx.subscribe(&map, |this: &mut Self, _, picked: &Picked, cx| {
            this.selection = Selection::from_pick(&picked.snapshot, picked.path);
            this.inspector_open = this.selection.is_some();
            this.refresh_detail(cx);
            cx.notify();
        });
        let connected = provider.is_some();
        Workspace {
            map,
            items: vec![Item {
                tag: ItemTag::Map,
                view: ItemView::Map,
            }],
            active: 0,
            selection: None,
            inspector_open: false,
            bench,
            provider: provider.unwrap_or_else(|| Rc::new(NullProvider)),
            connected,
            events: None,
            log: None,
            fetch_generation: 0,
            _pick_subscription: pick_subscription,
        }
    }

    // Every selection change invalidates whatever was in flight: replies race
    // clicks, and a stale answer must never land under a newer question.
    fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        self.fetch_generation += 1;
        self.events = None;
        self.log = None;
        if !self.connected {
            return;
        }
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let Some(namespace) = selection.namespace.as_deref() else {
            return;
        };
        let generation = self.fetch_generation;

        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_events(
            namespace,
            &selection.name,
            Box::new(move |detail| {
                let _ = tx.send(detail);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(detail) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.fetch_generation == generation {
                        this.events = Some(detail);
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        if selection.level == Level::Cell {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.provider.fetch_log_tail(
                namespace,
                &selection.name,
                Box::new(move |detail| {
                    let _ = tx.send(detail);
                }),
            );
            cx.spawn(async move |this, cx| {
                if let Ok(detail) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.fetch_generation == generation {
                            this.log = Some(detail);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    pub fn map_focus_handle(&self, cx: &App) -> FocusHandle {
        self.map.read(cx).focus_handle()
    }

    fn item_focus_handle(&self, index: usize, cx: &App) -> Option<FocusHandle> {
        match &self.items.get(index)?.view {
            ItemView::Map => Some(self.map.read(cx).focus_handle()),
            ItemView::Browse(view, _) => Some(view.read(cx).focus_handle()),
            ItemView::Text(view) => Some(view.read(cx).focus_handle()),
        }
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.items.len() {
            return;
        }
        self.active = index;
        if let Some(focus) = self.item_focus_handle(index, cx) {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn activate_existing(
        &mut self,
        tag: &ItemTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(index) = self.items.iter().position(|item| &item.tag == tag) {
            self.activate(index, window, cx);
            return true;
        }
        false
    }

    fn push_item(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        self.items.push(item);
        self.activate(self.items.len() - 1, window, cx);
    }

    fn subscribe_browse(
        &mut self,
        view: &Entity<BrowseView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            view,
            window,
            |this, _, event: &BrowseEvent, window, cx| match event {
                BrowseEvent::OpenDoc(request) => this.open_doc(request.clone(), window, cx),
                BrowseEvent::OpenLogs { namespace, pod } => {
                    this.open_logs(namespace.clone(), pod.clone(), window, cx)
                }
            },
        )
    }

    fn open_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.activate_existing(&ItemTag::Browse, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| BrowseView::kinds(provider, cx));
        let subscription = self.subscribe_browse(&view, window, cx);
        self.push_item(
            Item {
                tag: ItemTag::Browse,
                view: ItemView::Browse(view, subscription),
            },
            window,
            cx,
        );
    }

    fn open_nodes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.activate_existing(&ItemTag::Nodes, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| BrowseView::nodes(provider, cx));
        let subscription = self.subscribe_browse(&view, window, cx);
        self.push_item(
            Item {
                tag: ItemTag::Nodes,
                view: ItemView::Browse(view, subscription),
            },
            window,
            cx,
        );
    }

    fn open_doc(&mut self, request: DescribeRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tag = ItemTag::Doc(format!("{}/{}", request.uid, request.name));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| TextView::doc(provider, request, cx));
        self.push_item(
            Item {
                tag,
                view: ItemView::Text(view),
            },
            window,
            cx,
        );
    }

    fn open_logs(
        &mut self,
        namespace: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Logs(format!("{namespace}/{pod}"));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let request = LogRequest {
            namespace,
            pod,
            container: None,
            previous: false,
        };
        let view = cx.new(|cx| TextView::logs(provider, request, cx));
        self.push_item(
            Item {
                tag,
                view: ItemView::Text(view),
            },
            window,
            cx,
        );
    }

    fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active == 0 || self.items.len() <= 1 {
            return;
        }
        // Dropping the item drops its entity: a log follow's stop guard goes
        // with it, and the stream is cancelled.
        self.items.remove(self.active);
        let index = self.active.min(self.items.len() - 1);
        self.activate(index, window, cx);
    }

    fn item_title(&self, item: &Item, cx: &App) -> SharedString {
        match &item.view {
            ItemView::Map => "map".into(),
            ItemView::Browse(view, _) => view.read(cx).title(),
            ItemView::Text(view) => view.read(cx).title(),
        }
    }

    fn row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(PANEL_LABEL))
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(PANEL_VALUE))
                    .child(value.into()),
            )
    }

    fn inspector(&self, selection: Selection) -> impl IntoElement {
        let mut panel = div()
            .w(px(320.0))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(14.0))
            .bg(rgb(PANEL_BG))
            .border_l_1()
            .border_color(rgb(PANEL_BORDER))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(PANEL_HEADING))
                    .child(format!("{} {}", selection.kind, selection.name)),
            )
            .child(Self::row("name", selection.name.to_string()))
            .child(Self::row("kind", selection.kind));
        if let Some(namespace) = selection.namespace.as_deref() {
            panel = panel.child(Self::row("namespace", namespace.to_string()));
        }
        if let Some(owner) = selection.owner.as_deref() {
            panel = panel.child(Self::row("owner", owner.to_string()));
        }
        if !selection.uid.is_empty() {
            panel = panel.child(Self::row("uid", selection.uid.to_string()));
        }
        panel = panel.child(Self::detail_section(
            "events",
            self.events.as_ref(),
            |detail| {
                let Detail::Events(rows) = detail else {
                    return Vec::new();
                };
                rows.iter()
                    .map(|row| {
                        format!(
                            "{} x{} {} - {}",
                            row.kind, row.count, row.reason, row.message
                        )
                    })
                    .collect()
            },
        ));
        if selection.level == Level::Cell {
            panel = panel.child(Self::detail_section(
                "log tail",
                self.log.as_ref(),
                |detail| {
                    let Detail::Log(lines) = detail else {
                        return Vec::new();
                    };
                    lines.iter().rev().take(12).rev().cloned().collect()
                },
            ));
        }
        panel = panel.child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(PANEL_LABEL))
                .child(if self.connected {
                    "d describe · l logs (pods)"
                } else {
                    "no cluster connected; events and logs need one"
                }),
        );
        panel
    }

    fn detail_section(
        title: &'static str,
        detail: Option<&Detail>,
        rows: impl Fn(&Detail) -> Vec<String>,
    ) -> impl IntoElement {
        let mut section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(PANEL_LABEL))
                .child(title),
        );
        section = match detail {
            None => section.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(PANEL_LABEL))
                    .child("..."),
            ),
            Some(Detail::Denied(what)) => section.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(PANEL_HEADING))
                    .child(format!("{what}: access denied for this account")),
            ),
            Some(Detail::Failed(why)) => section.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(PANEL_HEADING))
                    .child(why.clone()),
            ),
            Some(detail) => {
                let lines = rows(detail);
                if lines.is_empty() {
                    section.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(PANEL_LABEL))
                            .child("none"),
                    )
                } else {
                    section.children(lines.into_iter().map(|line| {
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(PANEL_VALUE))
                            .child(line)
                    }))
                }
            }
        };
        section
    }

    fn tab_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let active = self.active;
        div()
            .h(px(26.0))
            .flex()
            .flex_row()
            .bg(rgb(TAB_BAR_BG))
            .border_b_1()
            .border_color(rgb(PANEL_BORDER))
            .children(self.items.iter().enumerate().map(|(index, item)| {
                let mut tab = div()
                    .px(px(12.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(if index == active {
                        rgb(TAB_TEXT)
                    } else {
                        rgb(TAB_DIM)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _: &MouseDownEvent, window, cx| {
                            this.activate(index, window, cx);
                        }),
                    )
                    .child(self.item_title(item, cx));
                if index == active {
                    tab = tab.bg(rgb(TAB_ACTIVE_BG));
                }
                tab
            }))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_is_map = matches!(
            self.items.get(self.active).map(|item| &item.view),
            Some(ItemView::Map) | None
        );
        let panel = (!self.bench && active_is_map && self.inspector_open)
            .then(|| self.selection.clone())
            .flatten();
        let content: gpui::AnyElement = match self.items.get(self.active).map(|item| &item.view) {
            Some(ItemView::Browse(view, _)) => view.clone().into_any_element(),
            Some(ItemView::Text(view)) => view.clone().into_any_element(),
            _ => self.map.clone().into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("Workspace")
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.inspector_open = !this.inspector_open && this.selection.is_some();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.selection.take().is_some() {
                    this.inspector_open = false;
                    this.refresh_detail(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenBrowser, window, cx| {
                this.open_browse(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenNodes, window, cx| {
                this.open_nodes(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DescribeSelection, window, cx| {
                if let Some(selection) = this.selection.clone() {
                    this.open_doc(selection.describe_request(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &LogsSelection, window, cx| {
                if let Some(selection) = this.selection.clone()
                    && selection.level == Level::Cell
                    && let Some(namespace) = selection.namespace.as_deref()
                {
                    this.open_logs(
                        namespace.to_string(),
                        selection.name.to_string(),
                        window,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &NextItem, window, cx| {
                let next = (this.active + 1) % this.items.len();
                this.activate(next, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevItem, window, cx| {
                let previous = (this.active + this.items.len() - 1) % this.items.len();
                this.activate(previous, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseItem, window, cx| {
                this.close_active(window, cx);
            }))
            .children((!self.bench).then(|| self.tab_bar(cx)))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .child(div().flex_1().min_w(px(0.0)).child(content))
                    .children(panel.map(|selection| self.inspector(selection))),
            )
    }
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
    fn every_binding_names_a_context_the_shell_actually_sets() {
        const CONTEXTS: [&str; 4] = ["Workspace", "Browse", "Doc", "Typing"];
        let bindings = keybindings();
        assert!(!bindings.is_empty());
        for binding in &bindings {
            let predicate = binding
                .predicate()
                .map(|p| p.to_string())
                .unwrap_or_default();
            assert!(
                CONTEXTS.contains(&predicate.as_str()),
                "a binding is scoped to an unknown context: {predicate:?}"
            );
        }
    }

    #[test]
    fn typing_mode_suppresses_every_plain_letter_the_workspace_binds() {
        let bindings = keybindings();
        let workspace_letters: Vec<String> = bindings
            .iter()
            .filter(|b| {
                b.predicate().map(|p| p.to_string()).as_deref() == Some("Workspace")
                    && b.keystrokes().len() == 1
                    && b.keystrokes()[0].modifiers().number_of_modifiers() == 0
                    && b.keystrokes()[0].key().chars().count() == 1
            })
            .map(|b| b.keystrokes()[0].key().to_string())
            .collect();
        assert!(!workspace_letters.is_empty());
        for letter in &workspace_letters {
            assert!(
                bindings.iter().any(|b| {
                    b.predicate().map(|p| p.to_string()).as_deref() == Some("Typing")
                        && b.keystrokes().len() == 1
                        && b.keystrokes()[0].key() == letter
                        && gpui::is_no_action(b.action())
                }),
                "typing a {letter:?} into a filter would dispatch a command instead"
            );
        }
    }
}
