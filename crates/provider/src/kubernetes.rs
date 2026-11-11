use k10s_core::KindId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Day2Op {
    Scale { current: i32, replicas: i32 },
    Restart,
    Pause,
    Resume,
    Delete,
    Evict,
    Cordon { unschedulable: bool },
    Drain { force: bool },
    Debug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Day2Request {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    pub op: Day2Op,
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Day2Outcome {
    Applied { summary: String, truncated: bool },
    Denied { what: &'static str, why: String },
    Rejected { message: String },
    Failed { why: String },
    NeedsConfirm { summary: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocOutcome {
    Doc { title: String, lines: Vec<String> },
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainersOutcome {
    Containers(Vec<String>),
    Denied(&'static str),
    Failed(String),
}

// One object as editable YAML text, with the identity the editor needs to
// resolve its schema. A Secret arrives structurally metadata-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOutcome {
    Manifest {
        title: String,
        yaml: String,
        api_version: String,
        kind: String,
        // The object as it was last declared, rendered by the same emitter:
        // the base document of a three-way diff. Absent on anything no
        // client-side apply ever wrote, which a diff has to say rather than
        // invent.
        last_applied: Option<String>,
        // From discovery, not guessed: whether the server takes a patch here
        // at all, and whether an apply may carry a status block.
        patchable: bool,
        status_subresource: bool,
        // Which object this text is, from the response that produced it. An
        // apply's answer carries the same field, and a server-side apply creates
        // what is absent, so comparing the two is what tells an update from a
        // recreation. Absent means the server sent none, which is neither
        // answer.
        uid: Option<String>,
    },
    Denied(&'static str),
    Failed(String),
}

// A server-side apply. The same request with `dry_run` set asks what the server
// *would* store and changes nothing, which is what makes an apply reviewable
// before it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    // Already stripped of everything the server owns.
    pub yaml: String,
    pub dry_run: bool,
    // Take the fields another manager holds. Only ever set once a conflict has
    // named them.
    pub force: bool,
}

// What a dry run's read found: the object, rendered like the editor's
// document, or nothing, so the apply would create it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCopy {
    Read { yaml: String, uid: Option<String> },
    Absent,
}

// One field another field manager owns, and who owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflicted {
    pub field: String,
    pub manager: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    // A real apply the server stored. `uid` is which object it answered about,
    // so the view can say whether the press updated the object it was opened
    // from or landed on a different one. None is "the answer carried no
    // identity", never "the same".
    Applied {
        uid: Option<String>,
    },
    // A dry run's answer, what the server would store, rendered like the
    // document the editor opened so the two can be diffed line for line, with
    // the object as the server held it a moment before it answered, read with
    // it: the review's left side. One variant, so an answer cannot arrive
    // without its read.
    DryRun {
        yaml: String,
        uid: Option<String>,
        live: LiveCopy,
    },
    // A dry run the server answered, whose answer cannot be shown: it is not
    // JSON, or the emitter will not render it (it caps a document at 2 MiB and
    // 64 levels). There is nothing to review. A real apply's answer is never
    // rendered, so it cannot end here.
    Unrendered {
        why: String,
    },
    Conflict {
        message: String,
        causes: Vec<Conflicted>,
        truncated: bool,
    },
    // The object moved since it was read. Forcing cannot help; it has to be read
    // again, which is why this is its own state and not a conflict with no
    // fields in it.
    Stale {
        message: String,
    },
    // The document itself was refused.
    Rejected {
        message: String,
        causes: Vec<String>,
    },
    // A write denial keeps the server's sentence: on a write a 403 is as often
    // an admission decision, which explains itself, as it is RBAC, which needs
    // no explaining.
    Denied {
        what: &'static str,
        why: String,
    },
    // A real apply that was sent and that nothing confirmed or refused: a lost
    // connection, the deadline, or a 5xx. It may have landed, which is why it is
    // not `Failed`; the next dry run reads the object with it and shows what is
    // there.
    Unconfirmed(String),
    Failed(String),
}

// One group-version the cluster serves schemas for, and the hash-stamped
// URL its document lives at. The URL is server data; the provider refuses
// any that escapes /openapi/v3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSource {
    pub group_version: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCatalogOutcome {
    Catalog(Vec<SchemaSource>),
    Denied(&'static str),
    Failed(String),
}

// Raw JSON text: an OpenAPI v3 document or a CRD list. Parsing lives in the
// editor engine; the seam only moves bounded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaTextOutcome {
    Text(String),
    Denied(&'static str),
    Failed(String),
}
