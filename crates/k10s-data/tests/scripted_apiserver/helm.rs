//! Helm releases read as release Secrets only, grouped newest first -- the one
//! place the Secret rule bends, held by what the list asks the server for.

use crate::*;

// Helm's release state is one Secret per revision and nothing else, so an
// inventory is a list and a decode. What this pins is the half a scripted server
// can prove: that the *request* is narrowed to release Secrets on the wire, that
// paging is followed, that revisions group into releases newest first, and that
// a payload which will not decode is counted rather than dropped.
#[test]
fn helm_releases_are_listed_as_release_secrets_only_and_grouped_newest_first() {
    use k10s_data::read::Fetched;

    fn payload(name: &str, revision: u32, status: &str) -> String {
        use base64::Engine;
        use std::io::Write;
        let json = format!(
            r#"{{"name":"{name}","namespace":"prod","version":{revision},
                 "info":{{"last_deployed":"2026-08-0{revision}T10:00:00Z","status":"{status}",
                          "description":"Upgrade complete","notes":"NOTES-SECRET"}},
                 "chart":{{"metadata":{{"name":"{name}","version":"4.11.{revision}",
                                       "appVersion":"1.11.{revision}"}},
                           "values":{{"password":"CHART-SECRET"}}}},
                 "config":{{"adminPassword":"USER-SECRET"}},
                 "manifest":"kind: Secret\ndata:\n  token: MANIFEST-SECRET\n"}}"#
        );
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(json.as_bytes()).expect("gzip");
        let engine = base64::engine::general_purpose::STANDARD;
        engine.encode(engine.encode(encoder.finish().expect("gzip")))
    }

    fn secret(name: &str, release: &str, revision: u32, status: &str) -> String {
        format!(
            r#"{{"metadata":{{"name":"{name}","namespace":"prod"}},
                 "type":"helm.sh/release.v1",
                 "data":{{"release":"{}"}}}}"#,
            payload(release, revision, status)
        )
    }

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    // Namespaced on purpose. Routes match by path prefix and are single-shot,
    // and the initial sync lists every discovered kind -- secrets among them --
    // cluster-wide, so a route for the cluster-wide collection would answer the
    // sync and leave this fetch reading the page after it. The two pages below
    // share a prefix and are answered in registration order, which is the order
    // this fetch asks for them in.
    script.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1",
                 "metadata":{{"continue":"page-2"}},
                 "items":[{},{}]}}"#,
            secret("sh.helm.release.v1.ingress.v1", "ingress", 1, "superseded"),
            secret("sh.helm.release.v1.ingress.v2", "ingress", 2, "deployed"),
        ),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1","metadata":{{}},
                 "items":[{},
                          {{"metadata":{{"name":"sh.helm.release.v1.broken.v1","namespace":"prod"}},
                            "type":"helm.sh/release.v1","data":{{"release":"not-a-payload"}}}},
                          {{"metadata":{{"name":"sh.helm.release.v1.empty.v1","namespace":"prod"}},
                            "type":"helm.sh/release.v1","data":{{}}}}]}}"#,
            secret("sh.helm.release.v1.api.v7", "api", 7, "failed"),
        ),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_releases(Some("prod".to_string()), move |fetched| {
            let _ = tx.send(fetched);
        });
    let outcome = wait(&rx);
    let Fetched::Ok(releases) = outcome else {
        panic!("the listing must resolve: {outcome:?}");
    };

    let names: Vec<(&str, &str)> = releases
        .releases
        .iter()
        .map(|release| (release.namespace.as_str(), release.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![("prod", "api"), ("prod", "ingress")],
        "one release per name, whatever its Secrets are called"
    );
    let ingress = &releases.releases[1];
    assert_eq!(
        ingress
            .revisions
            .iter()
            .map(|revision| revision.revision)
            .collect::<Vec<u32>>(),
        vec![2, 1],
        "newest first, so the running revision is the first element"
    );
    assert_eq!(
        ingress.current().expect("a current revision").status,
        "deployed"
    );
    assert_eq!(ingress.revisions[0].chart_version, "4.11.2");
    assert_eq!(
        releases.unreadable, 2,
        "a payload that will not decode and a Secret with none are counted, not dropped"
    );
    assert!(!releases.truncated);

    // The narrowing is on the wire, not in this process: no Secret that is not a
    // Helm release is ever transferred, which is what keeps this path inside the
    // rule that the read seam does not fetch Secret values.
    let lists = script.requests_for("/api/v1/namespaces/prod/secrets");
    assert_eq!(lists.len(), 2, "two pages, both asked for: {lists:?}");
    for request in &lists {
        assert!(
            request.path.contains("labelSelector=owner%3Dhelm"),
            "the label Helm writes: {}",
            request.path
        );
        assert!(
            request
                .path
                .contains("fieldSelector=type%3Dhelm.sh%2Frelease.v1"),
            "and its own Secret type: {}",
            request.path
        );
    }
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );

    // Nothing the payload carried beyond the inventory reaches a caller.
    let rendered = format!(
        "{releases:?}\n{}",
        k10s_data::helm::render(&releases).join("\n")
    );
    assert!(
        !rendered.contains("SECRET"),
        "the manifest, the values and the notes are dropped at the decode: {rendered}"
    );

    drop(runtime);
}
