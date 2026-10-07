//! The Traefik presence check asks only for release Secret metadata.

use crate::*;
use kube::api::{ListParams, Request};
use serde_json::Value;

const PATH: &str = "/api/v1/namespaces/kube-system/secrets?&fieldSelector=type%3Dhelm.sh%2Frelease.v1&labelSelector=owner%3Dhelm%2Cname%3Dtraefik";
const ACCEPT: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";

#[test]
fn traefik_presence_uses_strict_metadata_and_never_retries_for_secret_values() {
    let route = PATH.replacen("?&", "?", 1);
    for status in [200, 406] {
        let script = Script::default();
        script.route_accepting(
            "GET",
            &route,
            ACCEPT,
            status,
            if status == 200 {
                r#"{"kind":"PartialObjectMetadataList","apiVersion":"meta.k8s.io/v1","metadata":{},
                    "items":[{"metadata":{"name":"sh.helm.release.v1.traefik.v1","uid":"release-uid"}}]}"#
            } else {
                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":406,
                    "reason":"NotAcceptable","message":"Secret metadata representation unavailable"}"#
            },
        );
        script.route(
            "GET",
            &route,
            200,
            r#"{"kind":"SecretList","apiVersion":"v1","items":[{"metadata":{"name":"sh.helm.release.v1.traefik.v1"},"data":{"release":"PLANTED-full-secret-fallback"}}]}"#,
        );
        let params = ListParams::default()
            .labels("owner=helm,name=traefik")
            .fields("type=helm.sh/release.v1");
        let request = Request::new("/api/v1/namespaces/kube-system/secrets")
            .list_metadata(&params)
            .expect("the presence request");
        let runtime = runtime();
        let answer = runtime.block_on(async { script.client().request::<Value>(request).await });
        if status == 200 {
            let value = answer.unwrap_or_else(|error| {
                panic!(
                    "metadata satisfies the presence check: {error}; {:?}",
                    script.seen()
                )
            });
            assert_eq!(value["kind"], "PartialObjectMetadataList");
            assert_eq!(
                value["items"][0]["metadata"]["name"],
                "sh.helm.release.v1.traefik.v1"
            );
            assert!(value["items"][0].get("data").is_none());
        } else {
            let error = answer.expect_err("406 must not fall back to a full Secret list");
            assert!(
                matches!(error, kube::Error::Api(ref reply)
                    if reply.code == 406 && reply.message == "Secret metadata representation unavailable"),
                "{error}"
            );
        }
        let seen = script.seen();
        assert_eq!(seen.len(), 1, "no full-object fallback: {seen:?}");
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, PATH);
        assert_eq!(seen[0].accept, ACCEPT);
        assert_eq!(seen[0].content_type, "application/json");
        assert_eq!(seen[0].body, "");
    }
}
