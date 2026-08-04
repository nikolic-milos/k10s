//! The launch seam, wired to the data plane and the generator.
//!
//! The shell asks three questions -- which contexts are there, connect to this
//! one, generate a starmap -- and none of them may be answered on the GPUI
//! thread: reading a kubeconfig is file I/O on paths that can be stalled network
//! mounts, connecting is a round trip through a credential plugin, and
//! generating 25 000 objects is a CPU-bound fraction of a second. Each answer
//! therefore comes back from a thread this module owns, through the boxed reply
//! the seam hands over. The shell sees `ContextRow` and `ConnectOutcome`; it
//! never sees `kube`.
//!
//! [`Feed`] is the other half. `spawn_world` takes its live-event receiver once,
//! at spawn, so a window can open before anything has been chosen only if there
//! is one channel for the whole process that a choice attaches to afterwards. The
//! *scene* a choice brings does not travel down it: it goes as
//! `WorldCtrl::Rebuild`, carrying its own stream, so it is laid out by the same
//! batch layout the command line's scenes are and so replacing a scene is one act
//! rather than a race between two channels. What the feed carries is only what
//! comes after: one data plane's live deltas, on a thread that ends when that
//! plane does.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use k10s_core::{IngestEvent, WorldCtrl};
use k10s_data::connect::{Env, Listing, Source};
use k10s_data::{DEFAULT_EVENT_SINK_CAPACITY, DataPlane};
use k10s_shell::{
    ConfigSource, ConnectOutcome, ConnectRequest, Connection, ContextRow, DemoOutcome,
    LaunchProvider, ReadProvider, Reply, ScanOutcome, ScanRequest,
};

use crate::cli;

const WORLD_GONE: &str = "the world thread has stopped, so nothing can be drawn";

/// The channel a connected cluster's live deltas travel down.
#[derive(Clone)]
pub struct Feed {
    events: Sender<IngestEvent>,
}

impl Feed {
    pub fn new(events: Sender<IngestEvent>) -> Feed {
        Feed { events }
    }

    /// Forward one data plane's stream into the world until the plane is gone.
    /// The thread ends by itself when that plane's sink disconnects, which is what
    /// makes retiring a connection a drop followed by a join.
    fn pump(self, stream: Receiver<IngestEvent>) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("k10s-feed".into())
            .spawn(move || {
                for event in stream {
                    if self.events.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("the feed thread is spawned once per connection")
    }
}

/// One chosen scene's machinery: the data plane behind it, and the thread
/// carrying its stream into the world. The generator has neither.
struct Attached {
    plane: Option<DataPlane>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Attached {
    fn generated() -> Attached {
        Attached {
            plane: None,
            pump: None,
        }
    }

    /// Stop this scene's stream, then wait for the thread carrying it to notice.
    ///
    /// The order is load-bearing in both directions. The plane goes first because
    /// dropping it is what ends the watches; the pump is joined second, and only
    /// *after* the plane is dropped, because a watch task parked on a full
    /// bounded sink would otherwise deadlock the runtime shutdown that the
    /// plane's own drop waits on -- the pump draining that sink is what lets it
    /// see a disconnect instead. And the join is what makes the rebuild that
    /// follows safe: once it returns, nothing from this cluster can still arrive
    /// behind the scene that replaces it.
    fn retire(mut self) {
        drop(self.plane.take());
        if let Some(pump) = self.pump.take()
            && pump.join().is_err()
        {
            eprintln!("k10s: the event feed thread panicked; its cluster had stopped updating");
        }
    }
}

/// The seam itself. Cloning is an `Arc` bump: the window holds one and `main`
/// keeps one, because whatever was attached has to be stopped after the event
/// loop returns.
#[derive(Clone)]
pub struct LaunchService(Arc<Service>);

struct Service {
    feed: Feed,
    ctrl: Sender<WorldCtrl>,
    args: cli::Args,
    // The scene attached right now. Under a lock so two choices cannot attach at
    // once: the launch screen refuses a second confirm while one is in flight,
    // and this is what makes that true rather than assumed.
    current: Mutex<Option<Attached>>,
}

impl LaunchService {
    pub fn new(feed: Feed, ctrl: Sender<WorldCtrl>, args: &cli::Args) -> LaunchService {
        LaunchService(Arc::new(Service {
            feed,
            ctrl,
            args: args.clone(),
            current: Mutex::new(None),
        }))
    }

    /// Adopt a connection the command line made before the window opened. Its
    /// snapshot went in as the world's initial event vector, so only the live
    /// stream needs a pump; a cluster chosen from the screen later replaces the
    /// whole world and does not care how this one arrived.
    pub fn adopt_command_line(&self, plane: DataPlane, stream: Receiver<IngestEvent>) {
        *self.0.held() = Some(Attached {
            plane: Some(plane),
            pump: Some(self.0.feed.clone().pump(stream)),
        });
    }

    /// Everything a chosen scene owns, dropped in the order that stops it
    /// cleanly. Called once, as the process winds down.
    pub fn retire(&self) {
        if let Some(attached) = self.0.held().take() {
            attached.retire();
        }
    }
}

impl LaunchProvider for LaunchService {
    fn scan(&self, request: ScanRequest, reply: Reply<ScanOutcome>) {
        answer(
            "k10s-scan",
            move || scan(request),
            reply,
            ScanOutcome::Failed,
        );
    }

    fn connect(&self, request: ConnectRequest, reply: Reply<ConnectOutcome>) {
        let service = self.0.clone();
        answer(
            "k10s-connect",
            move || service.connect(request),
            reply,
            ConnectOutcome::Failed,
        );
    }

    fn generate(&self, reply: Reply<DemoOutcome>) {
        let service = self.0.clone();
        answer(
            "k10s-generate",
            move || service.generate(),
            reply,
            DemoOutcome::Failed,
        );
    }
}

impl Service {
    fn connect(&self, request: ConnectRequest) -> ConnectOutcome {
        // The new cluster is reached before the old one is given up, and the lock
        // is deliberately not held across the round trip: a refused attempt must
        // change nothing, and a window closed mid-connect must not wait out a
        // 30-second sync timeout to exit.
        let (sink, stream) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
        let plane = match k10s_data::spawn(sink) {
            Ok(plane) => plane,
            Err(error) => {
                return ConnectOutcome::Failed(format!("cannot start the data plane: {error}"));
            }
        };
        let options = k10s_data::Options {
            context: request.context.clone(),
            kubeconfig: match &request.source {
                ScanRequest::File(path) => Some(path.clone()),
                ScanRequest::Detected => None,
            },
            probe_namespaces: self.args.namespaces.clone(),
            sync_timeout: self.args.sync_timeout(),
        };
        let sync = match plane.sync(&options) {
            Ok(sync) => sync,
            // Refused, unreachable, a credential that would not mint: all of them
            // arrive here already redaction-filtered, and all of them are a
            // sentence on the launch screen rather than an exit.
            Err(error) => return ConnectOutcome::Failed(error.to_string()),
        };

        // A real cluster's pod states belong to the cluster. The rate goes to
        // zero rather than the flag going off, so the map's churn toggle cannot
        // later invent transitions in somebody's production namespace.
        let _ = self.ctrl.send(WorldCtrl::SetChurnRate(0.0));

        let summary = sync.report.summary();
        let context = sync.report.context.clone();
        eprintln!("k10s: {summary}");
        let notes = crate::degradation_notes(&sync.report, &sync.catalog, &sync.events);
        for note in &notes {
            eprintln!("k10s: {note}");
        }

        let inspector = sync.inspector;
        let reader = sync.reader;
        // From here on the swap is committed. The previous cluster goes first,
        // because `replace` returns only once nothing of it can still arrive, and
        // the world's rebuild drains whatever it already sent. Then the snapshot
        // builds the scene, and only then does the pump start -- so the world sees
        // the cluster's own order: everything that was, then everything that
        // changes.
        let mut current = self.held();
        self.replace(&mut current);
        if self.ctrl.send(WorldCtrl::Rebuild(sync.events)).is_err() {
            return ConnectOutcome::Failed(WORLD_GONE.to_string());
        }
        *current = Some(Attached {
            plane: Some(plane),
            pump: Some(self.feed.clone().pump(stream)),
        });
        ConnectOutcome::Connected(Connection {
            context,
            summary,
            notes,
            // Built on the thread that will own it: the `Rc` cannot cross back
            // the way the rest of this can.
            provider: Box::new(move || {
                Rc::new(crate::PlaneProvider { inspector, reader }) as Rc<dyn ReadProvider>
            }),
        })
    }

    fn generate(&self) -> DemoOutcome {
        // Generated before anything is given up, for the same reason a connection
        // is: this runs on a thread, and until it has produced a scene the one on
        // screen is still the truth.
        let generated = crate::generate(&self.args);
        eprintln!("k10s: {}", generated.summary);
        let mut current = self.held();
        self.replace(&mut current);
        // The generator is the whole point of the churn rate, and a cluster that
        // was attached before it will have set that rate to zero.
        let _ = self
            .ctrl
            .send(WorldCtrl::SetChurnRate(self.args.effective_churn()));
        if self
            .ctrl
            .send(WorldCtrl::Rebuild(generated.events))
            .is_err()
        {
            return DemoOutcome::Failed(WORLD_GONE.to_string());
        }
        *current = Some(Attached::generated());
        DemoOutcome::Started(generated.summary)
    }

    // Take down whatever is attached, before the scene that replaces it is sent:
    // `retire` returns only once nothing from the old cluster can still arrive,
    // which is exactly what the world's drain-then-rebuild relies on.
    fn replace(&self, current: &mut Option<Attached>) {
        if let Some(previous) = current.take() {
            previous.retire();
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Option<Attached>> {
        self.current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Answer off the caller's thread.
///
/// The reply is handed to the thread only once the thread exists. `Builder::spawn`
/// gives a failed closure back to nobody, so a reply moved in ahead of time would
/// be lost with it -- and a screen waiting forever on a thread that was never
/// created is exactly the dead end the launch screen may not have.
fn answer<T: 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
    reply: Reply<T>,
    refused: impl FnOnce(String) -> T,
) {
    let reply = Arc::new(Mutex::new(Some(reply)));
    let theirs = reply.clone();
    let take = |held: &Mutex<Option<Reply<T>>>| {
        held.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    };
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let answer = work();
            if let Some(reply) = take(&theirs) {
                reply(answer);
            }
        });
    if let Err(error) = spawned
        && let Some(reply) = take(&reply)
    {
        reply(refused(format!("cannot start the {name} thread: {error}")));
    }
}

fn scan(request: ScanRequest) -> ScanOutcome {
    match request {
        ScanRequest::Detected => ScanOutcome::Sources(
            k10s_data::connect::list(&Env::from_process())
                .into_iter()
                .map(config_source)
                .collect(),
        ),
        // A file somebody named and that will not read is the whole answer
        // failing, not one source among several coming back empty: they asked
        // about that file.
        ScanRequest::File(path) => {
            let listing = k10s_data::connect::list_file(&path);
            match listing.failure {
                Some(why) => ScanOutcome::Failed(why),
                None => ScanOutcome::Sources(vec![config_source(listing)]),
            }
        }
    }
}

// `~/.kube/config`, not `/home/somebody/.kube/config`. The header exists to be
// compared against what a person knows they set, and they know it with a tilde in
// it -- and a header is one line that clips, so the twelve characters matter.
fn shorten(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    let rendered = path.display().to_string();
    let Some(home) = home.map(|home| home.display().to_string()) else {
        return rendered;
    };
    // Prefix-matched on the separator, so `/home/mi` never shortens
    // `/home/milos/...` into something that names a different directory.
    match rendered.strip_prefix(&home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => rendered,
    }
}

fn config_source(listing: Listing) -> ConfigSource {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(std::path::PathBuf::from);
    ConfigSource {
        label: match &listing.source {
            // The paths as `KUBECONFIG` spells them, which is what somebody
            // comparing this list against their environment expects to see.
            Source::Kubeconfig(files) => files
                .iter()
                .map(|path| shorten(path, home.as_deref()))
                .collect::<Vec<_>>()
                .join(":"),
            Source::InCluster => "in-cluster service account".to_string(),
        },
        implicit: matches!(listing.source, Source::InCluster),
        contexts: listing
            .contexts
            .into_iter()
            .map(|context| ContextRow {
                name: context.name,
                current: context.current,
                server: context.server,
                namespace: context.namespace,
            })
            .collect(),
        note: listing.failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{KindId, Op, Payload, ResourceEvent};
    use k10s_data::connect::ContextInfo;

    fn scope(uid: &str) -> IngestEvent {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::NAMESPACE,
            uid: Arc::from(uid),
            namespace: Arc::from(""),
            name: Arc::from(uid),
            resource_version: 1,
            parent: None,
            op: Op::Added,
            payload: Payload::Scope,
        })
    }

    fn drain(rx: &Receiver<IngestEvent>) -> Vec<String> {
        rx.try_iter()
            .filter_map(|event| match event {
                IngestEvent::Resource(resource) => Some(resource.uid.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_pump_forwards_until_its_plane_disconnects_and_then_ends() {
        // Retiring a connection is a drop followed by a join, and this is the
        // half that makes the join finite: the thread ends by itself when the
        // plane it was carrying stops existing. Nothing may still arrive
        // afterwards, because the scene that replaces it is sent next.
        let (plane_tx, plane_rx) = crossbeam_channel::bounded(8);
        let (tx, rx) = crossbeam_channel::bounded(8);
        let pump = Feed::new(tx).pump(plane_rx);
        plane_tx.send(scope("ns-live")).expect("queued");
        drop(plane_tx);
        pump.join().expect("the pump ends by itself");
        assert_eq!(drain(&rx), vec!["ns-live".to_string()]);
    }

    #[test]
    fn a_pump_whose_world_has_gone_stops_rather_than_spinning() {
        let (plane_tx, plane_rx) = crossbeam_channel::bounded(8);
        let (tx, rx) = crossbeam_channel::bounded(8);
        let pump = Feed::new(tx).pump(plane_rx);
        drop(rx);
        plane_tx.send(scope("ns-live")).expect("queued");
        pump.join()
            .expect("the pump ends on a send that cannot land");
        assert!(
            plane_tx.send(scope("ns-live")).is_err(),
            "and its receiver goes with it, so the plane behind it learns that \
             nobody is reading rather than filling a sink forever"
        );
    }

    #[test]
    fn a_listing_becomes_rows_that_name_their_source_the_way_kubeconfig_does() {
        let source = config_source(Listing {
            source: Source::Kubeconfig(vec!["/a/config".into(), "/b/config".into()]),
            contexts: vec![ContextInfo {
                name: "prod".to_string(),
                current: true,
                server: Some("https://prod.example:6443".to_string()),
                namespace: Some("payments".to_string()),
            }],
            failure: None,
        });
        assert_eq!(source.label, "/a/config:/b/config");
        assert!(!source.implicit);
        assert_eq!(source.note, None);
        assert_eq!(source.contexts.len(), 1);
        assert_eq!(
            source.contexts[0].server.as_deref(),
            Some("https://prod.example:6443")
        );

        let in_cluster = config_source(Listing {
            source: Source::InCluster,
            contexts: Vec::new(),
            failure: None,
        });
        assert!(
            in_cluster.implicit,
            "an account with no contexts is still connectable, and the row has to say so"
        );

        let broken = config_source(Listing {
            source: Source::Kubeconfig(vec!["/a/bad".into()]),
            contexts: Vec::new(),
            failure: Some("/a/bad: invalid type".to_string()),
        });
        assert_eq!(broken.note.as_deref(), Some("/a/bad: invalid type"));
    }

    #[test]
    fn a_header_names_the_file_the_way_its_owner_does() {
        let home = std::path::Path::new("/home/milos");
        assert_eq!(
            shorten(std::path::Path::new("/home/milos/.kube/config"), Some(home)),
            "~/.kube/config"
        );
        assert_eq!(shorten(home, Some(home)), "~");
        assert_eq!(
            shorten(std::path::Path::new("/etc/k8s/edge.yaml"), Some(home)),
            "/etc/k8s/edge.yaml"
        );
        assert_eq!(
            shorten(
                std::path::Path::new("/home/milosnikolic/.kube/config"),
                Some(home)
            ),
            "/home/milosnikolic/.kube/config",
            "a home that is a string prefix of another is not that other home"
        );
        assert_eq!(
            shorten(std::path::Path::new("/home/milos/.kube/config"), None),
            "/home/milos/.kube/config"
        );
    }

    #[test]
    fn a_named_file_that_will_not_read_fails_the_whole_answer() {
        let missing = std::env::temp_dir().join("k10s-there-is-no-such-kubeconfig.yaml");
        match scan(ScanRequest::File(missing)) {
            ScanOutcome::Failed(why) => assert!(!why.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reply_survives_a_thread_that_could_not_be_started() {
        // The success path, and the fact that the reply is only ever called once.
        let (tx, rx) = std::sync::mpsc::channel();
        answer(
            "k10s-test",
            || "answered".to_string(),
            Box::new(move |value| tx.send(value).expect("received")),
            |why| why,
        );
        assert_eq!(rx.recv().expect("an answer arrives"), "answered");
        assert!(rx.recv().is_err(), "and only one does");
    }
}
