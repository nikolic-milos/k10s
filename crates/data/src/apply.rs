//! Server-side apply, dry run first.
//!
//! One request shape covers both halves of §5.2's last row: `?dryRun=All` asks
//! the API server what it *would* store and changes nothing, and the same
//! request without it stores it; a dry run reads the object first, so it is two
//! requests. The dry-run answer is the whole point of doing it this way -- it
//! is the authoritative right-hand side of a diff, computed by the machine that
//! owns the merge, admission webhooks and defaulting included, rather than by a
//! client guessing at strategic-merge semantics.
//!
//! The body is the buffer's own bytes under `application/apply-patch+yaml`,
//! whose entire purpose is that the bytes may be YAML: nothing in this process
//! parses YAML, so nothing in this process can disagree with the server about
//! what `on` or `y` means. `kube` builds the path, the query and the headers;
//! only the body is ours.
//!
//! One thing a server-side apply does that is worth stating rather than
//! discovering: it **creates** the object when it is absent. That is `kubectl
//! apply`'s behaviour too, and it means an apply of a document whose object was
//! deleted between the fetch and the press recreates it rather than failing --
//! which is not what the person pressing the key is thinking about. It is
//! detectable because the answer to the apply carries the object's `uid`:
//! [`Applied::uid`] and [`DryRun::uid`] are that field, and a caller that holds
//! the uid the document was *read* at can compare the two; a dry run's own read
//! carries one too. This layer reports the identity and draws no conclusion
//! from it; the conclusion needs the editor's uid, which lives with the review
//! rather than with the request. Secret responses are negotiated as metadata,
//! including on a dry run: the merged data fields must not enter the process to
//! be discarded later.
//!
//! `fieldManager=k10s` names us in `managedFields`, which is what makes a
//! conflict possible in the first place -- and a conflict is a labelled state
//! carrying the fields and the managers it would take them from, never a
//! flattened error string. Forcing it is a separate request the caller has to
//! ask for.
//!
//! Two of the outcomes below are shaped the way a real API server answers
//! rather than the way its status codes suggest, both established by
//! `tests/live_cluster.rs` against Kubernetes v1.36:
//!
//! - A 409 comes in two kinds. With `details.causes` it is a field-manager
//!   conflict and forcing takes the fields. *Without* them it is
//!   "Operation cannot be fulfilled ... the object has been modified" -- the
//!   object moved since it was read -- and forcing does not help at all, so
//!   offering it would be a lie. Those are two states, not one.
//! - Field validation is `Strict`, but that is not what catches a misspelled
//!   field in an apply: the field manager fails to build its typed patch first
//!   and answers **500** with `field not declared in schema`. So the text a
//!   person reads comes from the server's own `message` for every status code,
//!   never from a flattened error chain -- the server writes that sentence to
//!   be read, and it names the field. The dry run shows it as a failure; a real
//!   apply shows it as the reason the write is unconfirmed, since the same
//!   server answers its own 500 after storing a write too.
//!
//! And one outcome exists because a write is not the same event as a picture of
//! a write. A dry run's answer is rendered by the editor's own emitter, which
//! caps a document at 2 MiB and 64 levels of nesting, and a merged object can
//! exceed either -- so a dry run the server answered 2xx to can still fail to
//! render. That is [`ApplyOutcome::Unrendered`] and not [`ApplyOutcome::Failed`].
//! A real apply's answer is not rendered at all: only its uid is read, since
//! the editor that sent it reads the object again. A real apply that was sent
//! and got no verdict (a lost connection, the interactive deadline, or a 5xx,
//! which `connect::send_write` sorts out) is [`ApplyOutcome::Unconfirmed`]: it
//! may have landed, and nobody here can tell.
//!
//! So a review is never of a copy read earlier: every dry run reads the object
//! with it, on the same client, and [`DryRun::live`] is that read. A write that
//! landed before the read, this editor's or anyone's, is on the left side of
//! the review instead of hidden inside it. One that lands after it, between the
//! read and the dry run or later, is not; the caller compares the read's uid
//! with the answer's so a replacement in between is at least named.

use kube::Client;
use kube::api::{Patch, PatchParams, Request, ValidationDirective};
use kube::core::Status;

use k10s_core::KindId;

use crate::describe;
use crate::discover::KindTarget;
use crate::manifest;
use crate::read::collection_path;

pub const FIELD_MANAGER: &str = "k10s";

const MAX_CAUSES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    // The document to send, already stripped of everything the server owns.
    pub yaml: String,
    pub dry_run: bool,
    // Take ownership of the fields another manager holds. Only ever set after a
    // conflict named them.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    // Which object the server answered about. Compared against the uid the
    // document was read at, this is the difference between an update and a
    // recreation -- an apply creates what is absent, so the object a press lands
    // on need not be the object that was opened. Absent when the answer carried
    // no uid, and a caller must read that as "cannot tell" rather than as either
    // answer.
    pub uid: Option<String>,
}

// A dry run's answer and the read made with it: what the server would store,
// and the object as the server held it a moment before it answered, which is
// the review's left side. One value, so an answer without its read cannot be
// built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    pub yaml: String,
    pub uid: Option<String>,
    pub live: Live,
}

// What a dry run's read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    // Rendered by the same emitter as the answer, with the uid it carried.
    Read { yaml: String, uid: Option<String> },
    // It does not exist, so the apply would create it.
    Absent,
}

// A dry run the server answered 2xx to, whose answer cannot be shown: it is
// not JSON, or the editor's own emitter will not render it (it caps a document
// at 2 MiB and 64 levels of nesting, and the merged object the server echoes
// can exceed either). Separate from `Failed` because the server did answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrendered {
    pub why: String,
}

// One field another manager owns, and who owns it. The API server states the
// manager inside the cause's message; the field is its own attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflicted {
    pub field: String,
    pub manager: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied(Applied),
    DryRun(DryRun),
    // A dry run the server answered 2xx whose answer cannot be rendered here,
    // so there is nothing to review. A real apply's answer is never rendered.
    Unrendered(Unrendered),
    // 409 with causes: fields this apply sets are owned by someone else.
    // Forcing takes them; the causes say what and from whom.
    Conflict {
        message: String,
        causes: Vec<Conflicted>,
        // The server named more conflicts than the bounded review carries. A
        // caller must not offer force as though this were the complete set.
        truncated: bool,
    },
    // 409 with no causes: the object moved since it was read. Nothing can be
    // forced here -- the document has to be read again -- which is why this is
    // not a `Conflict` carrying an empty list.
    Stale {
        message: String,
    },
    // 400 or 422: the server rejected the document. With strict validation an
    // unknown or duplicated field lands here rather than being dropped.
    Rejected {
        message: String,
        causes: Vec<String>,
    },
    // 403. On a read that means RBAC and the label is the whole story; on a
    // write it is just as often an admission webhook, which says why -- and
    // discarding that sentence would leave someone editing an RBAC role that was
    // never the obstacle.
    Denied {
        what: &'static str,
        why: String,
    },
    // A real apply that was sent and that nothing confirmed or refused: the
    // connection broke after it was made, the interactive deadline passed, or
    // the answer was a redirect or a 5xx (`connect::NoVerdict`). The write may have
    // landed. `why` is the reason alone; what it means for the person who
    // pressed the key is the caller's sentence.
    Unconfirmed {
        why: String,
    },
    // A real apply that provably never left this process: its credential
    // failed, or no connection was made. Nothing was written. `why` is the
    // reason alone. Separate from `Failed` so a caller running several applies
    // can stop, since every one after it would fail the same way.
    NotSent {
        why: String,
    },
    // Nothing was written: the server refused it, or it was never built, or it
    // was a dry run. Everything that may have been written is `Unconfirmed`, and
    // a real apply that provably never left is `NotSent`.
    Failed {
        why: String,
    },
}

pub(crate) async fn apply(
    client: &Client,
    targets: &[KindTarget],
    request: &ApplyRequest,
) -> ApplyOutcome {
    let Some(target) = targets.iter().find(|target| target.id == request.kind) else {
        return ApplyOutcome::Failed {
            why: "this kind is not served by the connected cluster".to_string(),
        };
    };
    // What the server says about itself, before what this account may do: a
    // kind with no patch verb is not a permission problem and saying so saves
    // someone editing an RBAC role that was never the obstacle.
    if !target.patchable {
        return ApplyOutcome::Failed {
            why: format!(
                "the server serves {} without a patch verb, so it cannot be applied",
                target.kind()
            ),
        };
    }
    let params = PatchParams {
        dry_run: request.dry_run,
        force: request.force,
        field_manager: Some(FIELD_MANAGER.to_string()),
        field_validation: Some(ValidationDirective::Strict),
    };
    let api_request = Request::new(collection_path(target, request.namespace.as_deref()));
    // The value is a placeholder the body replaces; the variant supplies
    // kube's apply content type and query parameters.
    let patch = Patch::Apply(serde_json::Value::Null);
    let built = if crate::describe::is_secret(target) {
        api_request.patch_metadata(&request.name, &params, &patch)
    } else {
        api_request.patch(&request.name, &params, &patch)
    };
    let mut built = match built {
        Ok(built) => built,
        Err(error) => {
            return ApplyOutcome::Failed {
                why: error.to_string(),
            };
        }
    };
    *built.body_mut() = request.yaml.clone().into_bytes();
    let live = if request.dry_run {
        match read_live(client, target, request).await {
            Ok(live) => Some(live),
            Err(outcome) => return outcome,
        }
    } else {
        None
    };
    let response = match crate::connect::send_write(client, built).await {
        Ok(response) => response,
        Err(error) => return classify(&error, request.dry_run),
    };
    let value = serde_json::from_slice::<serde_json::Value>(response.body());
    // `live` is there exactly when this was a dry run: it was read above for
    // that and nothing else.
    let Some(live) = live else {
        // The write landed whatever the answer holds; an answer that is not
        // JSON only leaves its uid unknown.
        let uid = value.ok().and_then(|value| manifest::uid_of(&value));
        return ApplyOutcome::Applied(Applied { uid });
    };
    // The dry run reports whether it created or updated an object. A change
    // since the read leaves no valid left side for this comparison.
    if matches!(
        (&live, response.status()),
        (Live::Absent, http::StatusCode::OK) | (Live::Read { .. }, http::StatusCode::CREATED)
    ) {
        return ApplyOutcome::Failed {
            why: "the object's existence changed between the read and the dry run; compare again"
                .to_string(),
        };
    }
    let mut value = match value {
        Ok(value) => value,
        Err(why) => {
            return ApplyOutcome::Unrendered(Unrendered {
                why: format!("the answer is not JSON: {why}"),
            });
        }
    };
    // Read before rendering: the emitter is handed the value by mutable
    // reference, and the identity of what came back is not something a later
    // reader of the text should have to parse back out of it.
    let uid = manifest::uid_of(&value);
    match manifest::document(target, &mut value) {
        Ok((yaml, _)) => ApplyOutcome::DryRun(DryRun { yaml, uid, live }),
        Err(why) => ApplyOutcome::Unrendered(Unrendered {
            why: why.to_string(),
        }),
    }
}

// The object as it is now, read through the manifest path's own GET and
// rendering, so the two sides of a review differ only where the server's
// objects do. A read that cannot be made leaves the dry run nothing to be
// compared with, so its PATCH is not sent.
async fn read_live(
    client: &Client,
    target: &KindTarget,
    request: &ApplyRequest,
) -> Result<Live, ApplyOutcome> {
    let unread = |why: String| ApplyOutcome::Failed {
        why: format!("the object could not be read, so the dry run was not sent: {why}"),
    };
    match describe::fetch_object(client, target, request.namespace.as_deref(), &request.name).await
    {
        Ok(mut value) => {
            let uid = manifest::uid_of(&value);
            manifest::document(target, &mut value)
                .map(|(yaml, _)| Live::Read { yaml, uid })
                .map_err(|why| ApplyOutcome::Failed {
                    why: format!(
                        "the object as it is now cannot be shown here, so the dry run was not \
                         sent: {why}"
                    ),
                })
        }
        Err(kube::Error::Api(status)) if status.code == 404 => Ok(Live::Absent),
        Err(kube::Error::Api(status)) if matches!(status.code, 401 | 403) => {
            Err(ApplyOutcome::Denied {
                what: "read",
                why: format!(
                    "the dry run was not sent: {}",
                    crate::connect::status_sentence(&status)
                ),
            })
        }
        Err(kube::Error::Api(status)) => Err(unread(crate::connect::status_sentence(&status))),
        Err(error) => Err(unread(crate::connect::describe(
            &error as &(dyn std::error::Error + 'static),
        ))),
    }
}

// `dry_run` decides one thing: whether a request that got no verdict can have
// stored anything. A redirect, a 5xx or a connection lost after it was made is
// `Unconfirmed` on a real apply; a request whose credential or connection
// failed before sending is `NotSent`. A dry run stores nothing whatever
// happened, so there they are `Failed`. Every Api error carries the server's
// own sentence.
fn classify(error: &kube::Error, dry_run: bool) -> ApplyOutcome {
    if let kube::Error::Api(status) = error {
        match status.code {
            401 | 403 => {
                return ApplyOutcome::Denied {
                    what: "apply",
                    why: crate::connect::status_sentence(status),
                };
            }
            409 => {
                let (causes, truncated) = conflicts(status);
                return if !causes.is_empty() {
                    ApplyOutcome::Conflict {
                        message: crate::connect::status_sentence(status),
                        causes,
                        truncated,
                    }
                } else if status
                    .details
                    .as_ref()
                    .is_some_and(|details| !details.causes.is_empty())
                {
                    // A 409 cause is not automatically a field-manager conflict.
                    // Force only has defined meaning for FieldManagerConflict;
                    // every other structured refusal stays non-forceable.
                    ApplyOutcome::Rejected {
                        message: crate::connect::status_sentence(status),
                        causes: rejections(status),
                    }
                } else {
                    ApplyOutcome::Stale {
                        message: crate::connect::status_sentence(status),
                    }
                };
            }
            400 | 422 => {
                return ApplyOutcome::Rejected {
                    message: crate::connect::status_sentence(status),
                    causes: rejections(status),
                };
            }
            // Any other refusal, in the sentence it came with; the fallback
            // below is for a 5xx and for errors with no answer at all.
            _ => {
                return ApplyOutcome::Failed {
                    why: crate::connect::status_sentence(status),
                };
            }
        }
    }
    let error = error as &(dyn std::error::Error + 'static);
    let why = crate::connect::describe(error);
    if dry_run {
        ApplyOutcome::Failed { why }
    } else if crate::connect::never_sent(error) {
        ApplyOutcome::NotSent { why }
    } else {
        ApplyOutcome::Unconfirmed { why }
    }
}

fn conflicts(status: &Status) -> (Vec<Conflicted>, bool) {
    let mut conflicts: Vec<Conflicted> = status
        .details
        .iter()
        .flat_map(|details| details.causes.iter())
        .filter(|cause| cause.reason == "FieldManagerConflict")
        .take(MAX_CAUSES + 1)
        .map(|cause| Conflicted {
            field: cause.field.clone(),
            manager: manager_of(&cause.message),
        })
        .collect();
    let truncated = conflicts.len() > MAX_CAUSES;
    conflicts.truncate(MAX_CAUSES);
    (conflicts, truncated)
}

// The conflict cause reads `conflict with "kubectl" using apps/v1`, so the
// manager is whatever the first quoted run holds. A message shaped otherwise is
// carried whole rather than truncated to nothing: naming no manager at all would
// leave a person nothing to look for.
fn manager_of(message: &str) -> String {
    let mut parts = message.split('"');
    let (_, quoted) = (parts.next(), parts.next());
    match quoted {
        Some(manager) if !manager.is_empty() => manager.to_string(),
        _ => message.to_string(),
    }
}

fn rejections(status: &Status) -> Vec<String> {
    status
        .details
        .iter()
        .flat_map(|details| details.causes.iter())
        .take(MAX_CAUSES)
        .map(|cause| {
            if cause.field.is_empty() {
                cause.message.clone()
            } else {
                format!("{}: {}", cause.field, cause.message)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::response::{StatusCause, StatusDetails};

    fn status(code: u16, message: &str, causes: Vec<StatusCause>) -> kube::Error {
        let mut status = Status::failure(message, "Conflict").with_code(code);
        status.details = Some(StatusDetails {
            name: String::new(),
            group: String::new(),
            kind: String::new(),
            uid: String::new(),
            causes,
            retry_after_seconds: 0,
        });
        kube::Error::Api(status.boxed())
    }

    fn cause(field: &str, message: &str) -> StatusCause {
        StatusCause {
            reason: "FieldManagerConflict".to_string(),
            message: message.to_string(),
            field: field.to_string(),
        }
    }

    #[test]
    fn a_409_names_every_field_and_the_manager_holding_it() {
        let error = status(
            409,
            "Apply failed with 2 conflicts",
            vec![
                cause(".spec.replicas", "conflict with \"kubectl\" using apps/v1"),
                cause(
                    ".spec.template.spec.containers[name=\"web\"].image",
                    "conflict with \"argocd-controller\"",
                ),
            ],
        );
        let ApplyOutcome::Conflict {
            message,
            causes,
            truncated,
        } = classify(&error, true)
        else {
            panic!("a 409 is a conflict, not a failure");
        };
        assert!(!truncated);
        assert_eq!(message, "Apply failed with 2 conflicts");
        assert_eq!(
            causes,
            vec![
                Conflicted {
                    field: ".spec.replicas".to_string(),
                    manager: "kubectl".to_string(),
                },
                Conflicted {
                    field: ".spec.template.spec.containers[name=\"web\"].image".to_string(),
                    manager: "argocd-controller".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_conflict_message_naming_no_manager_is_carried_whole() {
        let error = status(409, "conflict", vec![cause(".spec", "someone else has it")]);
        let ApplyOutcome::Conflict { causes, .. } = classify(&error, true) else {
            panic!("a 409 is a conflict");
        };
        assert_eq!(causes[0].manager, "someone else has it");
    }

    // A 409 with no causes is the optimistic-lock kind, and forcing does
    // nothing for it: rendering it as a conflict would offer to take a list of
    // no fields from nobody.
    #[test]
    fn a_409_with_no_causes_is_staleness_rather_than_a_conflict() {
        let error = kube::Error::Api(
            Status::failure(
                "Operation cannot be fulfilled on configmaps \"settings\": the object has been modified",
                "Conflict",
            )
            .with_code(409)
            .boxed(),
        );
        let ApplyOutcome::Stale { message } = classify(&error, true) else {
            panic!("a causeless 409 is staleness");
        };
        assert!(message.contains("the object has been modified"));
    }

    #[test]
    fn a_409_with_non_field_manager_causes_is_never_forceable() {
        let error = status(
            409,
            "the requested operation conflicts with another change",
            vec![StatusCause {
                reason: "FieldValueInvalid".to_string(),
                message: "the precondition no longer holds".to_string(),
                field: "metadata.uid".to_string(),
            }],
        );
        let ApplyOutcome::Rejected { causes, .. } = classify(&error, true) else {
            panic!("a non-field-manager cause must stay non-forceable");
        };
        assert_eq!(
            causes,
            vec!["metadata.uid: the precondition no longer holds"]
        );
    }

    // What the lab's server answers a misspelled field in an apply, recorded: the
    // typed patch cannot be built, a 500 with no reason whose message names the
    // field. A 5xx is no verdict, since the same server answers its own 500 after
    // storing a write too, so the sentence reaches a person as it is: the failure
    // of a dry run, and the reason a real apply is unconfirmed.
    #[test]
    fn a_misspelled_field_reaches_a_person_as_the_servers_own_sentence() {
        let message = "failed to create typed patch object (g2/settings; /v1, Kind=ConfigMap): \
                       .dataz: field not declared in schema";
        let answer =
            || kube::Error::Service(Box::new(crate::connect::NoVerdict(message.to_string())));
        assert_eq!(
            classify(&answer(), true),
            ApplyOutcome::Failed {
                why: message.to_string()
            }
        );
        assert_eq!(
            classify(&answer(), false),
            ApplyOutcome::Unconfirmed {
                why: message.to_string()
            }
        );
    }

    // A transport failure says nothing at all, and a real apply may already
    // have been served, so it is `Unconfirmed` rather than a failure that implies
    // the cluster is untouched. A dry run stores nothing either way.
    #[test]
    fn a_real_apply_that_lost_its_connection_is_unconfirmed_and_a_dry_run_failed() {
        let broken = || {
            kube::Error::ReadEvents(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ))
        };
        let ApplyOutcome::Unconfirmed { why } = classify(&broken(), false) else {
            panic!("a real apply with no answer may have landed");
        };
        assert!(why.contains("connection reset by peer"), "{why}");
        assert!(
            !why.contains("landed"),
            "the reason alone; the caller words what it means: {why}"
        );

        let ApplyOutcome::Failed { why } = classify(&broken(), true) else {
            panic!("a dry run with no answer stored nothing");
        };
        assert!(why.contains("connection reset by peer"), "{why}");
    }

    // An answer that is no verdict (a redirect or a 5xx, whoever wrote it) says
    // nothing of the request behind it. On a real apply
    // that is the same uncertainty as a lost connection, and its sentence is
    // the reason.
    #[test]
    fn an_answer_with_no_verdict_is_unconfirmed_on_a_real_apply_and_a_failure_on_a_dry_run() {
        let message = "Timeout: request did not complete within requested timeout";
        let timeout =
            || kube::Error::Service(Box::new(crate::connect::NoVerdict(message.to_string())));
        assert_eq!(
            classify(&timeout(), false),
            ApplyOutcome::Unconfirmed {
                why: message.to_string()
            }
        );
        assert_eq!(
            classify(&timeout(), true),
            ApplyOutcome::Failed {
                why: message.to_string()
            }
        );
    }

    // The interactive client's deadline travels inside kube's `Error::Service`;
    // it reaches the person as its own sentence, not as "ServiceError: ...".
    #[test]
    fn a_real_apply_past_the_deadline_is_unconfirmed_in_the_deadlines_words() {
        let late = kube::Error::Service(Box::new(crate::deadline::NoAnswer(
            std::time::Duration::from_secs(35),
        )));
        assert_eq!(
            classify(&late, false),
            ApplyOutcome::Unconfirmed {
                why: "no answer within 35 s".to_string()
            }
        );
    }

    // A body that was not a Status (a gateway's text, API Priority and
    // Fairness's "Too many requests, please try again later.\n", recorded on the
    // lab) arrives as its raw text; the sentence carries no trailing newline.
    #[test]
    fn a_plain_text_answer_is_its_sentence_without_the_newline() {
        let error = kube::Error::Api(
            Status::failure("Too many requests, please try again later.\n", "")
                .with_code(429)
                .boxed(),
        );
        assert_eq!(
            classify(&error, false),
            ApplyOutcome::Failed {
                why: "Too many requests, please try again later.".to_string()
            }
        );
    }

    #[test]
    fn a_status_with_no_message_names_its_code_rather_than_printing_nothing() {
        let error = kube::Error::Api(Status::failure("", "").with_code(405).boxed());
        let ApplyOutcome::Failed { why } = classify(&error, true) else {
            panic!("a 405 is a labelled failure");
        };
        assert_eq!(why, "answered with status 405 and no message");
    }

    #[test]
    fn strict_validation_rejections_name_their_fields() {
        let error = status(
            400,
            "strict decoding error",
            vec![
                cause("", "unknown field \"spec.replicaz\""),
                cause("spec.paused", "duplicate field"),
            ],
        );
        let ApplyOutcome::Rejected { causes, .. } = classify(&error, true) else {
            panic!("a 400 is a rejection, not a failure");
        };
        assert_eq!(
            causes,
            vec![
                "unknown field \"spec.replicaz\"".to_string(),
                "spec.paused: duplicate field".to_string(),
            ]
        );
    }

    #[test]
    fn a_refused_write_is_a_denial_that_keeps_the_servers_reason() {
        // An admission webhook's refusal arrives as a 403 whose message is the
        // only thing that says what to change.
        let denied = kube::Error::Api(
            Status::failure(
                "admission webhook \"policy.example.com\" denied the request: every workload needs a runAsNonRoot",
                "Forbidden",
            )
            .with_code(403)
            .boxed(),
        );
        let ApplyOutcome::Denied { what, why } = classify(&denied, true) else {
            panic!("a 403 is a denial");
        };
        assert_eq!(what, "apply");
        assert!(
            why.contains("runAsNonRoot"),
            "the reason survives the labelling: {why}"
        );

        let expired = kube::Error::Api(
            Status::failure("the token has expired", "Unauthorized")
                .with_code(401)
                .boxed(),
        );
        let ApplyOutcome::Denied { what, why } = classify(&expired, true) else {
            panic!("a 401 is the same refusal a read calls denied, not a generic failure");
        };
        assert_eq!(what, "apply");
        assert_eq!(why, "the token has expired");

        // The lab's answer to a write over the body limit, recorded.
        let too_large = kube::Error::Api(
            Status::failure(
                "Request entity too large: limit is 3145728",
                "RequestEntityTooLarge",
            )
            .with_code(413)
            .boxed(),
        );
        assert_eq!(
            classify(&too_large, false),
            ApplyOutcome::Failed {
                why: "Request entity too large: limit is 3145728".to_string()
            },
            "any other refusal is a failure in the server's words"
        );
    }

    // A pathological server could answer with thousands of causes; the panel
    // that renders them is bounded like every other buffer in the crate.
    #[test]
    fn the_cause_list_is_capped() {
        let many: Vec<StatusCause> = (0..MAX_CAUSES + 10)
            .map(|at| cause(&format!(".spec.field{at}"), "conflict with \"other\""))
            .collect();
        let error = status(409, "many", many);
        let ApplyOutcome::Conflict {
            causes, truncated, ..
        } = classify(&error, true)
        else {
            panic!("a 409 is a conflict");
        };
        assert_eq!(causes.len(), MAX_CAUSES);
        assert!(truncated, "force must know the review omitted conflicts");
    }
}
