//! Server-side apply, dry run first.
//!
//! One request shape covers both halves of §5.2's last row: `?dryRun=All` asks
//! the API server what it *would* store and changes nothing, and the same
//! request without it stores it. The dry-run answer is the whole point of doing
//! it this way -- it is the authoritative right-hand side of a diff, computed by
//! the machine that owns the merge, admission webhooks and defaulting
//! included, rather than by a client guessing at strategic-merge semantics.
//!
//! The body is the buffer's own bytes under `application/apply-patch+yaml`,
//! whose entire purpose is that the bytes may be YAML: nothing in this process
//! parses YAML, so nothing in this process can disagree with the server about
//! what `on` or `y` means. `kube` builds the path, the query and the headers;
//! only the body is ours.
//!
//! One thing a server-side apply does that is worth stating rather than
//! discovering: it **creates** the object when it is absent. That is
//! `kubectl apply`'s behaviour too, and it means an apply of a document whose
//! object was deleted between the fetch and the press recreates it rather than
//! failing -- which is not what the person pressing the key is thinking about.
//! It is detectable with no second round trip, because the answer to the apply
//! carries the object's `uid`: [`Applied::uid`] is that field, and a caller that
//! holds the uid the document was *read* at can compare the two. This layer
//! reports the identity and draws no conclusion from it; the conclusion needs
//! the read's uid, which lives with the review rather than with the request.
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
//!   be read, and it names the field.
//!
//! And one outcome exists because a write is not the same event as a picture of
//! a write. The response body is rendered by the editor's own emitter, which
//! caps a document at 2 MiB and 64 levels of nesting, and a merged object can
//! exceed either -- so a request the server answered 2xx to can still fail to
//! render. That is [`ApplyOutcome::Unrendered`] and not [`ApplyOutcome::Failed`]
//! because on a real apply the object is already stored: calling it a failure
//! tells someone the cluster is unchanged while it holds their write.

use kube::Client;
use kube::api::{Patch, PatchParams, Request, ValidationDirective};
use kube::core::Status;

use k10s_core::KindId;

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
    // What the server would store, or did, rendered by the same emitter the
    // editor opened the object with.
    pub yaml: String,
    pub dry_run: bool,
    // Which object the server answered about. Compared against the uid the
    // document was read at, this is the difference between an update and a
    // recreation -- an apply creates what is absent, so the object a press lands
    // on need not be the object that was opened. Absent when the answer carried
    // no uid, and a caller must read that as "cannot tell" rather than as either
    // answer.
    pub uid: Option<String>,
}

// A request the server answered 2xx to, whose answer the editor's own emitter
// will not render -- it caps a document at 2 MiB and 64 levels of nesting, and
// the merged object the server echoes can exceed either. Separate from
// `Failed` because on a real apply the write *landed*: reporting it as a
// failure tells someone the cluster is unchanged when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrendered {
    pub dry_run: bool,
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
    // The server took it and answered; only the rendering of that answer
    // failed. On a real apply this is a write that happened.
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
    // Nothing was written, as far as anything here can tell. Everything that
    // *was* written and could not be shown is `Unrendered` instead.
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
    let built = Request::new(collection_path(target, request.namespace.as_deref())).patch(
        &request.name,
        &params,
        // The value is a placeholder the body replaces; what this variant is
        // here for is the `application/apply-patch+yaml` content type and the
        // query parameters, both of which are kube's own to spell.
        &Patch::Apply(serde_json::Value::Null),
    );
    let mut built = match built {
        Ok(built) => built,
        Err(error) => {
            return ApplyOutcome::Failed {
                why: error.to_string(),
            };
        }
    };
    *built.body_mut() = request.yaml.clone().into_bytes();
    match client.request::<serde_json::Value>(built).await {
        Ok(mut value) => {
            // Read before rendering: the emitter is handed the value by mutable
            // reference, and the identity of what came back is not something a
            // later reader of the text should have to parse back out of it.
            let uid = manifest::uid_of(&value);
            match manifest::document(target, &mut value) {
                Ok((yaml, _)) => ApplyOutcome::Applied(Applied {
                    yaml,
                    dry_run: request.dry_run,
                    uid,
                }),
                // The server answered, so on a real apply the object is already
                // stored. Only the emitter refused it -- it caps a document at
                // 2 MiB and 64 levels, and a merged object can exceed either.
                // Calling that a failure would tell someone their write did not
                // happen while the cluster holds it.
                Err(why) => ApplyOutcome::Unrendered(Unrendered {
                    dry_run: request.dry_run,
                    why: why.to_string(),
                }),
            }
        }
        Err(error) => classify(&error, request.dry_run),
    }
}

// `dry_run` is here for one case: an error that never reached a server, on a
// request that may already have been served. A 4xx or 5xx is the server
// speaking and says for itself that nothing was stored; a transport failure
// says nothing at all, and on a real apply the honest answer names that
// instead of implying the cluster is untouched.
fn classify(error: &kube::Error, dry_run: bool) -> ApplyOutcome {
    if let kube::Error::Api(status) = error {
        match status.code {
            403 => {
                return ApplyOutcome::Denied {
                    what: "apply",
                    why: message_of(status),
                };
            }
            409 => {
                let (causes, truncated) = conflicts(status);
                return if !causes.is_empty() {
                    ApplyOutcome::Conflict {
                        message: message_of(status),
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
                        message: message_of(status),
                        causes: rejections(status),
                    }
                } else {
                    ApplyOutcome::Stale {
                        message: message_of(status),
                    }
                };
            }
            400 | 422 => {
                return ApplyOutcome::Rejected {
                    message: message_of(status),
                    causes: rejections(status),
                };
            }
            // Anything else the server answered, it answered in a sentence it
            // wrote for a person; the chain-walking fallback below is for the
            // errors that never reached a server at all.
            _ => {
                return ApplyOutcome::Failed {
                    why: message_of(status),
                };
            }
        }
    }
    let why = crate::connect::describe(error as &(dyn std::error::Error + 'static));
    if dry_run {
        return ApplyOutcome::Failed { why };
    }
    ApplyOutcome::Failed {
        why: format!(
            "{why}; the apply may or may not have reached the cluster, so read the object again \
             before sending it a second time"
        ),
    }
}

// The API server's own `message`, which it writes to be read. A status that
// somehow carries none names its code rather than printing an empty line.
fn message_of(status: &Status) -> String {
    if status.message.is_empty() {
        return format!(
            "the API server refused the apply with status {}",
            status.code
        );
    }
    status.message.clone()
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

    // What a real server answers for a misspelled field in an apply: the typed
    // patch cannot be built, so it is a 500 whose message names the field. It
    // has to reach a person as that sentence, not as a flattened error chain.
    #[test]
    fn an_unexpected_status_still_carries_the_servers_own_sentence() {
        let error = kube::Error::Api(
            Status::failure(
                "failed to create typed patch object (g2/settings; /v1, Kind=ConfigMap): .dataz: field not declared in schema",
                "InternalError",
            )
            .with_code(500)
            .boxed(),
        );
        let ApplyOutcome::Failed { why } = classify(&error, true) else {
            panic!("a 500 is a labelled failure");
        };
        assert_eq!(
            why,
            "failed to create typed patch object (g2/settings; /v1, Kind=ConfigMap): .dataz: field not declared in schema",
            "the field is named and nothing is wrapped around it"
        );
    }

    // A status code is the server speaking: it says for itself that nothing was
    // stored. A transport failure says nothing at all, and on a real apply the
    // request may already have been served -- so the sentence must not imply
    // the cluster is untouched.
    #[test]
    fn an_error_that_never_reached_a_server_does_not_claim_the_write_did_not_happen() {
        let broken = || {
            kube::Error::ReadEvents(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ))
        };
        let ApplyOutcome::Failed { why } = classify(&broken(), false) else {
            panic!("a transport failure is a labelled failure");
        };
        assert!(why.contains("connection reset by peer"), "{why}");
        assert!(
            why.contains("may or may not have reached the cluster"),
            "and it says what it cannot know: {why}"
        );

        let ApplyOutcome::Failed { why } = classify(&broken(), true) else {
            panic!("a transport failure is a labelled failure");
        };
        assert!(
            !why.contains("may or may not"),
            "a dry run had nothing to store either way: {why}"
        );
    }

    #[test]
    fn a_status_with_no_message_names_its_code_rather_than_printing_nothing() {
        let error = kube::Error::Api(Status::failure("", "").with_code(507).boxed());
        let ApplyOutcome::Failed { why } = classify(&error, true) else {
            panic!("a 507 is a labelled failure");
        };
        assert_eq!(why, "the API server refused the apply with status 507");
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
    fn a_403_on_a_write_is_a_denial_that_keeps_the_servers_reason() {
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

        let broken = kube::Error::Api(
            Status::failure("etcdserver: request timed out", "ServiceUnavailable")
                .with_code(503)
                .boxed(),
        );
        let ApplyOutcome::Failed { why } = classify(&broken, true) else {
            panic!("a 503 is a labelled failure");
        };
        assert_eq!(why, "etcdserver: request timed out");
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
