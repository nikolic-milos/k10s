//! Decoding a `helm.sh/release.v1` Secret through both of its layers, and the
//! reduction that makes the rest of the payload unreachable: nothing a release
//! carries beyond its inventory survives, and a compression bomb is refused
//! rather than expanded.

use super::*;
use std::io::Write;

// A payload shaped the way Helm's own client writes one, including the parts
// this module must not carry.
fn release_json(name: &str, revision: u32, status: &str) -> String {
    format!(
        r#"{{"name":"{name}","namespace":"prod","version":{revision},
               "info":{{"first_deployed":"2026-07-01T09:00:00Z",
                        "last_deployed":"2026-08-01T10:22:31Z",
                        "status":"{status}","description":"Upgrade complete",
                        "notes":"NOTES.txt says SUPERSECRET-NOTES"}},
               "chart":{{"metadata":{{"name":"ingress-nginx","version":"4.11.3",
                                     "appVersion":"1.11.3"}},
                         "values":{{"password":"SUPERSECRET-DEFAULT"}}}},
               "config":{{"adminPassword":"SUPERSECRET-USER"}},
               "manifest":"apiVersion: v1\nkind: Secret\ndata:\n  token: SUPERSECRET-MANIFEST\n",
               "hooks":[{{"manifest":"SUPERSECRET-HOOK"}}]}}"#
    )
}

fn gzipped(json: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(json.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

// How the bytes look in a raw read of the Secret: Helm base64s its gzip, and
// the API server base64s the value it is given.
fn as_the_api_server_sends_it(json: &str) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    engine.encode(engine.encode(gzipped(json)))
}

#[test]
fn a_release_payload_decodes_through_both_layers_of_base64() {
    let stored = decode(&as_the_api_server_sends_it(&release_json(
        "ingress-nginx",
        4,
        "deployed",
    )))
    .expect("a Helm payload decodes");
    assert_eq!(stored.name, "ingress-nginx");
    assert_eq!(stored.namespace, "prod");
    assert_eq!(stored.revision.revision, 4);
    assert_eq!(stored.revision.status, "deployed");
    assert_eq!(stored.revision.updated, "2026-08-01T10:22:31Z");
    assert_eq!(stored.revision.description, "Upgrade complete");
    assert_eq!(stored.revision.chart, "ingress-nginx");
    assert_eq!(stored.revision.chart_version, "4.11.3");
    assert_eq!(stored.revision.app_version, "1.11.3");
}

// Every other spelling the same bytes arrive in. The shape decides, so a
// typed client's single layer and a Helm that stopped compressing both read.
#[test]
fn one_layer_of_base64_and_an_uncompressed_payload_read_the_same_way() {
    let json = release_json("api", 1, "deployed");
    let engine = base64::engine::general_purpose::STANDARD;
    for encoded in [
        engine.encode(gzipped(&json)),
        engine.encode(&json),
        engine.encode(engine.encode(&json)),
    ] {
        let stored = decode(&encoded).expect("the shape decides, not the layering");
        assert_eq!(stored.revision.revision, 1);
    }
}

// The invariant this module exists to keep. A release payload carries the
// rendered manifest, the user's values and the chart's defaults, and any of
// those can hold a password: what is *returned* has nowhere to put them.
#[test]
fn nothing_a_release_carries_beyond_its_inventory_survives_the_decode() {
    let json = release_json("ingress-nginx", 4, "deployed");
    assert!(
        json.contains("SUPERSECRET"),
        "the fixture has to contain what must not come out"
    );
    let stored = decode(&as_the_api_server_sends_it(&json)).expect("decodes");
    let releases = Releases {
        releases: vec![Release {
            name: stored.name,
            namespace: stored.namespace,
            revisions: vec![stored.revision],
        }],
        truncated: false,
        unreadable: 0,
    };
    let rendered = format!("{releases:?}\n{}", render(&releases).join("\n"));
    assert!(
        !rendered.contains("SUPERSECRET"),
        "the manifest, the values and the notes are dropped at the boundary: {rendered}"
    );
}

#[test]
fn a_payload_that_is_not_a_release_is_a_reason_rather_than_a_panic() {
    assert!(decode("not base64 at all !!").is_err());
    let engine = base64::engine::general_purpose::STANDARD;
    assert!(decode(&engine.encode([0x00, 0x01, 0x02])).is_err());
    assert!(
        decode(&engine.encode(engine.encode("[1,2,3]"))).is_err(),
        "JSON that is not an object is not a release"
    );
    // Valid gzip of something that is not a release document.
    assert!(decode(&engine.encode(gzipped("{\"unrelated\":1}"))).is_ok());
}

// A few hundred bytes that name gigabytes. Refused, not truncated, and not
// read into memory first.
#[test]
fn a_compression_bomb_is_refused_rather_than_expanded() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let block = vec![b'a'; 1 << 20];
    for _ in 0..(MAX_PAYLOAD_BYTES / block.len()) + 2 {
        encoder.write_all(&block).unwrap();
    }
    let bomb = encoder.finish().unwrap();
    assert!(
        bomb.len() < 64 << 10,
        "the point is that the compressed side is small: {} bytes",
        bomb.len()
    );
    let engine = base64::engine::general_purpose::STANDARD;
    assert_eq!(
        decode(&engine.encode(bomb)).err(),
        Some("this release's payload is larger than this view decodes")
    );
}

// The payload cap is not a field cap: one 8 MiB status inside a legal payload
// set the padded width of every revision line of its release, so 2,000 short
// revisions became 2,000 strings of 8 MiB. Bounded where the field is carried,
// so the type cannot hold it rather than the renderer having to remember.
#[test]
fn one_enormous_field_cannot_set_the_width_of_every_line() {
    // One huge field inside a payload that is comfortably *legal*: that is
    // the whole point, since a payload over the cap is already refused and
    // this one is not.
    let huge = "a".repeat(6 << 20);
    let json = format!(
        r#"{{"name":"x","namespace":"prod","version":1,
                 "info":{{"status":"{huge}","description":"d","last_deployed":"t"}},
                 "chart":{{"metadata":{{"name":"c","version":"1","appVersion":"1"}}}}}}"#
    );
    let engine = base64::engine::general_purpose::STANDARD;
    let stored = decode(&engine.encode(gzipped(&json))).expect("a legal payload");
    for field in [
        &stored.name,
        &stored.revision.status,
        &stored.revision.updated,
        &stored.revision.description,
        &stored.revision.chart,
        &stored.revision.chart_version,
        &stored.revision.app_version,
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
    }
    assert!(
        stored.revision.status.ends_with('\u{2026}'),
        "and looks clipped"
    );

    let release = Release {
        name: stored.name,
        namespace: stored.namespace,
        revisions: (0..64).map(|_| stored.revision.clone()).collect(),
    };
    let rendered = render(&Releases {
        releases: vec![release],
        truncated: false,
        unreadable: 0,
    });
    let widest = rendered.iter().map(String::len).max().unwrap_or(0);
    assert!(
        widest < 4_000,
        "so no line can be multiplied out by the revision count: {widest} bytes"
    );
}

// Every release Secret failed to decode. Saying the cluster holds none
// contradicts the field that counted them, and it is the loudest line here.
#[test]
fn an_inventory_that_could_not_read_anything_does_not_claim_there_is_nothing() {
    let lines = render(&Releases {
        releases: Vec::new(),
        truncated: false,
        unreadable: 3,
    });
    let text = lines.join("\n");
    assert!(
        !text.contains("no Helm releases are stored"),
        "three are stored and were seen: {text}"
    );
    assert!(lines[0].contains("though some are stored"), "{text}");
    assert!(
        text.contains("3 release secrets could not be decoded"),
        "{text}"
    );
}

#[test]
fn an_empty_inventory_says_what_it_looked_at_rather_than_nothing() {
    let lines = render(&Releases::default());
    assert_eq!(lines[0], "no Helm releases are stored in this cluster");
    let text = lines.join("\n");
    assert!(text.contains("helm.sh/release.v1"), "{text}");
    assert!(
        text.contains("ConfigMap storage driver"),
        "an empty answer names the reasons it could be wrong: {text}"
    );
}

#[test]
fn the_status_column_is_measured_the_way_the_formatter_pads_it() {
    let revision = |revision: u32, status: &str| Revision {
        revision,
        status: status.to_string(),
        updated: String::new(),
        description: String::new(),
        chart: "chart".to_string(),
        chart_version: "1.0.0".to_string(),
        app_version: String::new(),
    };
    let lines = render(&Releases {
        releases: vec![Release {
            name: "app".to_string(),
            namespace: "prod".to_string(),
            revisions: vec![revision(2, "déployé"), revision(1, "ok")],
        }],
        truncated: false,
        unreadable: 0,
    });
    let column = |line: &str| {
        let at = line.find("chart-1.0.0").expect("the chart column");
        line[..at].chars().count()
    };
    let rows: Vec<&String> = lines.iter().filter(|line| line.contains("rev ")).collect();
    assert_eq!(rows.len(), 2, "{lines:?}");
    assert!(
        rows[0].contains("déployé  chart-1.0.0"),
        "the formatter pads in characters, so measuring the widest status in bytes \
         opens a gap as wide as its multibyte excess: {rows:?}"
    );
    assert_eq!(
        column(rows[0]),
        column(rows[1]),
        "and the narrower one is padded to the same character column: {rows:?}"
    );
}

#[test]
fn a_history_renders_newest_first_with_its_chart_and_status() {
    let revision = |revision: u32, status: &str| Revision {
        revision,
        status: status.to_string(),
        updated: "2026-08-01T10:22:31Z".to_string(),
        description: "Upgrade complete".to_string(),
        chart: "ingress-nginx".to_string(),
        chart_version: "4.11.3".to_string(),
        app_version: "1.11.3".to_string(),
    };
    let release = Release {
        name: "ingress-nginx".to_string(),
        namespace: "prod".to_string(),
        revisions: vec![revision(4, "deployed"), revision(3, "superseded")],
    };
    assert_eq!(release.current().map(|current| current.revision), Some(4));
    let lines = render(&Releases {
        releases: vec![release],
        truncated: true,
        unreadable: 2,
    });
    let text = lines.join("\n");
    assert!(text.starts_with("1 release, 2 stored revisions"), "{text}");
    assert!(text.contains("prod/ingress-nginx"), "{text}");
    assert!(
        text.contains("rev 4    deployed    ingress-nginx-4.11.3  app 1.11.3"),
        "the status column is padded to the widest status: {text}"
    );
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("2 release secrets could not be decoded and are not shown"),
        "and so is a payload that would not read: {text}"
    );
    assert!(
        text.contains("values and rendered manifests are not shown"),
        "and the reason the obvious next thing is missing: {text}"
    );
}
