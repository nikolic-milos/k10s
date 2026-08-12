//! What the editor reads, what it writes, and the schema it fetches.
//!
//! The three dispatchers [`crate::editor::EditorSource`] exists for: reload,
//! save, and schema scope. A new kind of source extends that enum and these
//! three, and touches nothing else.
//!
//! Nothing here blocks the UI thread. Reads, writes, and even the conflict
//! stamp run on the background executor, and every non-`Ok` outcome becomes a
//! labelled note rather than a silent absence. Writes go through
//! [`crate::saves::SaveQueue`] -- one in flight, only the newest text queued
//! behind it -- because a write per keypress leaves the order to the executor,
//! and an older write can land last on a buffer already marked clean.

use std::path::PathBuf;

use gpui::Context;

use k10s_edit::complete::doc_meta;
use k10s_edit::{Buffer, DocMeta, LanguageKind, Syntax};

use crate::dirty::{CloseStep, SaveStep};
use crate::editor::{EditorEvent, EditorSource, EditorView, SchemaStore, file_title};
use crate::fs::Stamp;
use crate::provider::{
    DescribeRequest, ManifestOutcome, Reply, SchemaCatalogOutcome, SchemaTextOutcome,
};
use crate::saves::PendingSave;

impl EditorView {
    // Fetch the source into the buffer. A buffer that has never been loaded is
    // dirty by definition -- it has no clean point yet -- so opening must not go
    // through the guard below, or every file opens empty behind a warning about
    // unsaved changes it does not have.
    pub(crate) fn load(&mut self, cx: &mut Context<Self>) {
        match self.source.clone() {
            EditorSource::Cluster(request) => self.reload_cluster(request, cx),
            EditorSource::File(path) => self.reload_file(path, cx),
            EditorSource::Scratch => {
                self.status = Some("this buffer has no file to reload from".to_string());
                cx.notify();
            }
        }
    }

    // The reload action, which throws unsaved work away and so asks first. It
    // also empties the save queue: a save queued behind the in-flight one holds
    // the text the user has just chosen to discard, and writing it afterwards
    // would put the discarded version on disk.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        if self.is_dirty() && self.dirty.reload_step(self.buffer.version()) == CloseStep::Warn {
            self.status = Some("unsaved changes; reload again to discard them".to_string());
            cx.notify();
            return;
        }
        self.saves.abandon();
        self.load(cx);
    }

    fn reload_cluster(&mut self, request: DescribeRequest, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = Some("loading...".to_string());
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<ManifestOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.fetch_manifest(&request, reply);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match outcome {
                        ManifestOutcome::Manifest {
                            title,
                            yaml,
                            api_version,
                            kind,
                            last_applied,
                            patchable,
                            status_subresource,
                            uid,
                        } => {
                            this.title = title.into();
                            this.buffer = Buffer::new(&yaml);
                            this.syntax.reparse(this.buffer.rope());
                            this.live = Some(yaml);
                            this.base = last_applied;
                            this.uid = uid;
                            this.patchable = patchable;
                            this.status_subresource = status_subresource;
                            this.meta = DocMeta {
                                api_version: Some(api_version),
                                kind: Some(kind),
                            };
                            this.dirty.mark_clean(this.buffer.version(), None);
                            this.scroll_top = 0;
                            this.status = None;
                            this.resync_search();
                            this.publish_state(cx);
                            this.ensure_schema(cx);
                            this.schedule_validation(cx);
                        }
                        ManifestOutcome::Denied(what) => {
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        ManifestOutcome::Failed(why) => this.status = Some(why),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn reload_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = Some("loading...".to_string());
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let text = fs.read_to_string(&path)?;
                    let stamp = fs.stamp(&path)?;
                    Ok::<(String, Stamp), std::io::Error>((text, stamp))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match loaded {
                    Ok((text, stamp)) => {
                        this.buffer = Buffer::new(&text);
                        this.syntax.reparse(this.buffer.rope());
                        this.refresh_meta();
                        this.dirty.mark_clean(this.buffer.version(), Some(stamp));
                        this.scroll_top = 0;
                        this.status = None;
                        this.resync_search();
                        this.publish_state(cx);
                        this.ensure_schema(cx);
                        this.schedule_validation(cx);
                    }
                    // A file that is not there yet is not a failure when the
                    // caller brought a template for exactly that case.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match this.template {
                            Some(template) => {
                                this.buffer = Buffer::new(template);
                                this.syntax.reparse(this.buffer.rope());
                                this.refresh_meta();
                                this.dirty.forget_disk();
                                this.scroll_top = 0;
                                this.status = Some("new file; ctrl-s writes it".to_string());
                                this.resync_search();
                                this.publish_state(cx);
                                this.ensure_schema(cx);
                                this.schedule_validation(cx);
                            }
                            None => this.status = Some(format!("open failed: {error}")),
                        }
                    }
                    Err(error) => this.status = Some(format!("open failed: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn refresh_meta(&mut self) {
        let cursor = self.buffer.primary_selection().head;
        let document = self.syntax.document_index_at(self.buffer.rope(), cursor);
        self.meta = doc_meta(self.buffer.rope(), &self.syntax, document);
    }

    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        match self.source.clone() {
            EditorSource::File(path) => self.save_to(path, cx),
            EditorSource::Scratch => {
                cx.emit(EditorEvent::SaveAsRequested);
            }
            // A cluster document is never written blind: ctrl-s asks the server
            // what it would store and opens that answer as a diff, and the
            // apply is a second, deliberate press inside it.
            EditorSource::Cluster(_) => {
                cx.emit(EditorEvent::DiffRequested { dry_run: true });
            }
        }
    }

    // The picker hands back a path: the buffer adopts it as its file, takes
    // its language from the extension, and saves.
    pub fn assign_path_and_save(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.title = file_title(&path).into();
        let language = LanguageKind::from_file_name(&self.title);
        if language != self.syntax.language() {
            self.syntax = Syntax::new(language);
            self.syntax.reparse(self.buffer.rope());
            self.schedule_validation(cx);
        }
        self.source = EditorSource::File(path.clone());
        // A fixed root is bound to a file's identity: once this buffer is a
        // different file, the settings or keymap schema is not its schema.
        self.schema_root = None;
        // The identity changed, so anything already in flight describes the
        // previous file and must not answer for this one.
        self.generation += 1;
        self.dirty.forget_disk();
        self.save_to(path, cx);
        self.reported_dirty = self.is_dirty();
        cx.emit(EditorEvent::StateChanged);
    }

    pub(crate) fn save_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let request = PendingSave {
            path,
            text: self.buffer.text(),
            version: self.buffer.version(),
            generation: self.generation,
            ticket: 0,
        };
        // A save already running owns the file, and it already answered the
        // conflict question; this text queues behind it.
        let Some(start) = self.saves.request(request) else {
            self.status = Some("saving...".to_string());
            cx.notify();
            return;
        };
        self.begin_save(start, cx);
    }

    pub(crate) fn begin_save(&mut self, save: PendingSave, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self.status = Some("saving...".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            // Stamping the file is a syscall, and on a network mount it is not
            // one a frame can wait for, so even the conflict check happens off
            // the UI thread.
            let stamped = {
                let fs = fs.clone();
                let path = save.path.clone();
                cx.background_executor()
                    .spawn(async move { fs.stamp(&path).ok() })
                    .await
            };
            let cleared = this.update(cx, |this, cx| {
                if save.generation != this.generation {
                    // The buffer this text came from is gone, so its conflict
                    // question is not one to ask. Whatever was asked for since
                    // owns the queue.
                    if let Some(next) = this.saves.finished(save.ticket) {
                        this.begin_save(next, cx);
                    }
                    return false;
                }
                match this.dirty.save_step(stamped) {
                    SaveStep::Confirm(reason) => {
                        // The user answers before anything is written, and the
                        // queue empties so the answer is a deliberate press
                        // rather than whatever was typed ahead.
                        this.saves.abandon();
                        this.status = Some(reason.note().to_string());
                        cx.notify();
                        false
                    }
                    SaveStep::Write => true,
                }
            });
            if !matches!(cleared, Ok(true)) {
                return;
            }
            let written = {
                let path = save.path.clone();
                let text = save.text.clone();
                cx.background_executor()
                    .spawn(async move {
                        fs.write(&path, &text)?;
                        // The write is what mattered; a stamp we cannot read
                        // only costs us the conflict check on the next save.
                        Ok::<Option<Stamp>, std::io::Error>(fs.stamp(&path).ok())
                    })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                // Save-as adopts a new path, and a reload replaces the buffer,
                // while the previous write is still in flight. That write was
                // asked for and is correct, but its result describes a document
                // this view is no longer showing: marking that version clean
                // hands the tab a false answer, and adopting the old file's
                // stamp invents a conflict on the next save.
                let still_ours = save.generation == this.generation
                    && matches!(&this.source, EditorSource::File(path) if *path == save.path);
                match written {
                    Ok(stamp) if still_ours => {
                        this.dirty.mark_clean(save.version, stamp);
                        this.status = stamp.is_none().then(|| {
                            "saved, but the file cannot be stamped; the next save will \
                             ask before overwriting"
                                .to_string()
                        });
                    }
                    Ok(_) => {}
                    Err(error) => this.status = Some(format!("save failed: {error}")),
                }
                this.publish_state(cx);
                cx.notify();
                if let Some(next) = this.saves.finished(save.ticket) {
                    this.begin_save(next, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn ensure_schema(&mut self, cx: &mut Context<Self>) {
        if self.schema_root.is_some() {
            return;
        }
        let fetch_catalog = {
            let mut store = self.schema.borrow_mut();
            !std::mem::replace(&mut store.requested_catalog, true)
        };
        if fetch_catalog {
            let (tx, rx) = futures::channel::oneshot::channel();
            let reply: Reply<SchemaCatalogOutcome> = Box::new(move |outcome| {
                let _ = tx.send(outcome);
            });
            self.provider.fetch_schema_catalog(reply);
            cx.spawn(async move |this, cx| {
                if let Ok(outcome) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        {
                            absorb_catalog(&mut this.schema.borrow_mut(), outcome);
                        }
                        this.ensure_documents(cx);
                        this.schedule_validation(cx);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        let fetch_crds = {
            let mut store = self.schema.borrow_mut();
            !std::mem::replace(&mut store.requested_crds, true)
        };
        if fetch_crds {
            let (tx, rx) = futures::channel::oneshot::channel();
            let reply: Reply<SchemaTextOutcome> = Box::new(move |outcome| {
                let _ = tx.send(outcome);
            });
            self.provider.fetch_crd_schemas(reply);
            cx.spawn(async move |this, cx| {
                if let Ok(outcome) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        {
                            absorb_schema_text(
                                &mut this.schema.borrow_mut(),
                                outcome,
                                SchemaDoc::CrdList,
                            );
                        }
                        this.schedule_validation(cx);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        self.ensure_documents(cx);
    }

    pub(crate) fn ensure_documents(&mut self, cx: &mut Context<Self>) {
        let Some(api_version) = self.meta.api_version.clone() else {
            return;
        };
        let Some(url) = next_document_url(&mut self.schema.borrow_mut(), &api_version) else {
            return;
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<SchemaTextOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.fetch_schema_document(&url, reply);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    {
                        absorb_schema_text(
                            &mut this.schema.borrow_mut(),
                            outcome,
                            SchemaDoc::OpenApi,
                        );
                    }
                    this.schedule_validation(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

// How a catalog answer lands in the store: every source's group version is
// registered with the index BEFORE the catalog is stored, so a document the
// index cannot name is never offered for fetching; and a denial is worded for
// the person reading the notes, not for the transport.
pub(crate) fn absorb_catalog(store: &mut SchemaStore, outcome: SchemaCatalogOutcome) {
    match outcome {
        SchemaCatalogOutcome::Catalog(sources) => {
            for source in &sources {
                store.index.add_api_version(&source.group_version);
            }
            store.catalog = sources;
        }
        SchemaCatalogOutcome::Denied(what) => {
            store.note(format!("{what}: access denied for this account"))
        }
        SchemaCatalogOutcome::Failed(why) => store.note(why),
    }
}

// Which schema text an answer carries; the Denied and Failed arms are shared
// and only the destination of the Text arm differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchemaDoc {
    OpenApi,
    CrdList,
}

pub(crate) fn absorb_schema_text(
    store: &mut SchemaStore,
    outcome: SchemaTextOutcome,
    kind: SchemaDoc,
) {
    match outcome {
        SchemaTextOutcome::Text(json) => {
            let landed = match kind {
                SchemaDoc::OpenApi => store.index.add_openapi_document(&json),
                SchemaDoc::CrdList => store.index.add_crd_list(&json),
            };
            if let Err(why) = landed {
                store.note(match kind {
                    SchemaDoc::OpenApi => format!("schema document: {why}"),
                    SchemaDoc::CrdList => format!("CRD schemas: {why}"),
                });
            }
        }
        SchemaTextOutcome::Denied(what) => {
            store.note(format!("{what}: access denied for this account"))
        }
        SchemaTextOutcome::Failed(why) => store.note(why),
    }
}

// The document fetch a group version still owes, at most once per store: the
// store is per-workspace precisely so a second editor on the same group
// version fetches nothing, and `requested_documents` is where that promise is
// kept.
pub(crate) fn next_document_url(store: &mut SchemaStore, api_version: &str) -> Option<String> {
    let source = store
        .catalog
        .iter()
        .find(|source| source.group_version == api_version)
        .cloned()?;
    store
        .requested_documents
        .insert(source.group_version)
        .then_some(source.url)
}
