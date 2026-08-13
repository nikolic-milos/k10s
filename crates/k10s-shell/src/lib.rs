//! The app shell: the workspace that hosts the Starmap and everything docked
//! around it.
//!
//! The shell owns selection, actions, items, and panels; the map stays a view
//! that paints snapshots and emits picks. State crosses this boundary as
//! values -- a `Picked` carries the exact snapshot the user clicked on, and a
//! [`Selection`] is derived from it by a pure function, so a panel can never
//! disagree with the frame that was on screen. The center is a row of items
//! -- the map, the kind browser, the node capacity table, describe documents,
//! live log follows -- switched by tabs and keyed actions; anything hosted
//! implements [`Item`] and the workspace holds it as a boxed [`ItemHandle`],
//! so a new panel kind never touches workspace internals. Every read goes
//! through the [`ReadProvider`] seam, so the shell never sees kube, and every
//! denial arrives as a labelled state; the local terminal is the same
//! `TerminalView` on a PTY transport instead of an exec. The chrome is
//! Zed's: a title bar with the application menu, drag-to-move, and
//! client-side window controls when the compositor asks for them, and a
//! status bar whose panel buttons dispatch the same actions the keys do.
//! Keybindings are scoped by context (`Workspace`, `Browse`, `Doc`,
//! `Typing`), with text-typing keystrokes -- plain and shifted -- suppressed
//! while an input mode is capturing. Panels and items render on notify only:
//! zero paints at idle is a gated invariant and the shell must never be the
//! reason it fails. The left edge is a thin activity rail of chrome icons;
//! clicking one opens the matching dock or item. Brand glyphs stay on the map.
//!
//! [`Workspace`] is one type described by five modules, split by what each is
//! about rather than by what it is made of: `workspace` holds the state and the
//! overlays, `hosting` puts items in panes, `cluster` chooses and adopts a
//! connection, `chrome` draws the furniture and `render` assembles it. The parts
//! with no gpui in them at all -- the default keymap, the pick-to-selection
//! function, item identity, and the tab arithmetic under both the centre row and
//! the docks -- are their own modules precisely so they can be tested without a
//! window.

pub mod attribution;
pub mod browse;
pub mod config_schema;
pub mod diff;
pub mod diff_gate;
#[cfg(test)]
mod diff_test;
pub mod dock;
pub mod editor;
pub mod files;
pub mod finder;
#[cfg(test)]
mod finder_test;
pub mod forwards;
pub mod fs;
pub mod item;
pub mod keymap;
pub mod launch;
pub mod palette;
pub mod provider;
#[cfg(unix)]
pub mod pty;
pub mod reveal;
pub mod saved_views;
pub mod settings;
pub mod table;
pub mod term;
pub mod text;
pub mod ui;

// The shell's own parts. Private, because the crate's surface is the views it
// hosts and the seams it reads through -- everything a consumer needs from here
// is re-exported below under the name it has always had.
mod actions;
mod activity;
mod bindings;
mod chrome;
mod cluster;
mod dirty;
mod editor_element;
mod editor_io;
#[cfg(test)]
mod editor_io_test;
mod hosting;
mod modal;
mod overlay;
mod pane;
mod render;
mod saves;
mod selection;
mod spans;
mod tag;
mod workspace;

pub use actions::*;
pub use bindings::{input_suppressors, keybindings};
pub use item::{Item, ItemHandle};
pub use provider::{
    ApplyOutcome, ApplyRequest, Bytes, ConfigSource, Conflicted, ConnectOutcome, ConnectRequest,
    Connection, ContainersOutcome, ContextRow, DemoOutcome, DescribeRequest, Detail, DocOutcome,
    EventRow, ExecEvent, ExecRequest, ExecSession, ForwardOutcome, ForwardRequest, ForwardRow,
    ForwardState, KindRow, LaunchProvider, LogChunk, LogRequest, LogStop, ManifestOutcome,
    Millicores, NullExecSession, NullLaunchProvider, NullProvider, OverlayOutcome, OverlayStamp,
    PodPostureView, PostureOutcome, ProviderFactory, ProviderSlot, ReadProvider, Reply,
    ScanOutcome, ScanRequest, SchemaCatalogOutcome, SchemaSource, SchemaTextOutcome, TableColumn,
    TableOutcome, TablePage, TableRow, UsageOutcome, UsageRequest, UsageSample, UsageSource,
    UsageTarget, WorkloadLogRequest,
};
pub use selection::{LogTarget, Selection};
pub use workspace::{ConfigPaths, Workspace};
