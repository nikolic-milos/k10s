//! The cluster's own schema sources: `/openapi/v3` and CRD schemas, fetched
//! raw and bounded.
//!
//! This module only moves JSON text across the seam -- parsing and indexing
//! live in the editor engine, which the scripted API server cannot reach.
//! The `/openapi/v3` index maps group-versions to hash-stamped document
//! URLs; those URLs are server data, so a document fetch refuses any path
//! that escapes `/openapi/v3` -- the prefix has to end on a segment boundary
//! and no segment may walk back out of it, which a bare `starts_with` would
//! let through twice over. A server that predates the endpoint is a
//! labelled failure, a 403 is a denial, and an absent CRD API degrades to
//! an empty list, because a cluster without CRDs is normal and a cluster
//! that hides them should still complete built-in kinds. Documents are
//! capped in size: schema text is untrusted input from whoever wrote the
//! CRD.

use std::collections::HashMap;

use kube::Client;
use serde::Deserialize;

use crate::read::{Fetched, classify};

const MAX_SOURCES: usize = 1024;
const MAX_DOCUMENT_BYTES: usize = 16 << 20;
const OPENAPI_ROOT: &str = "/openapi/v3";
const CRD_PATH: &str = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions";
pub(crate) const EMPTY_CRD_LIST: &str = r#"{"items":[]}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSource {
    pub group_version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct WireIndex {
    #[serde(default)]
    paths: HashMap<String, WirePath>,
}

#[derive(Deserialize)]
struct WirePath {
    #[serde(rename = "serverRelativeURL", default)]
    server_relative_url: String,
}

fn get(path: &str) -> Result<http::Request<Vec<u8>>, http::Error> {
    http::Request::get(path).body(Vec::new())
}

pub(crate) async fn fetch_catalog(client: &Client) -> Fetched<Vec<SchemaSource>> {
    let request = match get(OPENAPI_ROOT) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: "schema catalog",
                why: error.to_string(),
            };
        }
    };
    let index: WireIndex = match client.request(request).await {
        Ok(index) => index,
        Err(kube::Error::Api(response)) if response.code == 404 => {
            return Fetched::Failed {
                what: "schema catalog",
                why: "this server does not serve /openapi/v3".to_string(),
            };
        }
        Err(error) => return classify("schema catalog", &error),
    };
    Fetched::Ok(sources_from(index))
}

fn sources_from(index: WireIndex) -> Vec<SchemaSource> {
    let mut sources: Vec<SchemaSource> = index
        .paths
        .into_iter()
        .filter_map(|(path, wire)| {
            let group_version = path
                .strip_prefix("apis/")
                .or_else(|| path.strip_prefix("api/"))?;
            if group_version.is_empty() || !under_openapi_root(&wire.server_relative_url) {
                return None;
            }
            Some(SchemaSource {
                group_version: group_version.to_string(),
                url: wire.server_relative_url,
            })
        })
        .collect();
    sources.sort_by(|a, b| a.group_version.cmp(&b.group_version));
    sources.truncate(MAX_SOURCES);
    sources
}

fn under_openapi_root(url: &str) -> bool {
    const UPWARDS: [&str; 4] = ["..", "%2e%2e", "%2e.", ".%2e"];
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = path.strip_prefix(OPENAPI_ROOT) else {
        return false;
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return false;
    }
    !rest
        .split('/')
        .any(|segment| UPWARDS.iter().any(|up| segment.eq_ignore_ascii_case(up)))
}

pub(crate) async fn fetch_document(client: &Client, url: &str) -> Fetched<String> {
    if !under_openapi_root(url) {
        return Fetched::Failed {
            what: "schema document",
            why: "refused a schema URL outside /openapi/v3".to_string(),
        };
    }
    let request = match get(url) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: "schema document",
                why: error.to_string(),
            };
        }
    };
    match client.request_text(request).await {
        Ok(text) if text.len() > MAX_DOCUMENT_BYTES => Fetched::Failed {
            what: "schema document",
            why: "the schema document exceeds 16 MiB".to_string(),
        },
        Ok(text) => Fetched::Ok(text),
        Err(error) => classify("schema document", &error),
    }
}

pub(crate) async fn fetch_crds(client: &Client) -> Fetched<String> {
    let request = match get(CRD_PATH) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: "CRD schemas",
                why: error.to_string(),
            };
        }
    };
    match client.request_text(request).await {
        Ok(text) if text.len() > MAX_DOCUMENT_BYTES => Fetched::Failed {
            what: "CRD schemas",
            why: "the CRD list exceeds 16 MiB".to_string(),
        },
        Ok(text) => Fetched::Ok(text),
        Err(kube::Error::Api(response)) if response.code == 404 => {
            Fetched::Ok(EMPTY_CRD_LIST.to_string())
        }
        Err(error) => classify("CRD schemas", &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_shape_maps_paths_to_group_versions() {
        let index: WireIndex = serde_json::from_str(
            r#"{"paths":{
                "api/v1":{"serverRelativeURL":"/openapi/v3/api/v1?hash=aaa"},
                "apis/apps/v1":{"serverRelativeURL":"/openapi/v3/apis/apps/v1?hash=bbb"},
                "apis/example.com/v1":{"serverRelativeURL":"/openapi/v3/apis/example.com/v1?hash=ccc"},
                "logs":{"serverRelativeURL":"/logs"},
                "apis/evil":{"serverRelativeURL":"/etc/passwd"},
                "apis/evil/v2":{"serverRelativeURL":"/openapi/v3/../../../etc/passwd"},
                "apis/evil/v3":{"serverRelativeURL":"/openapi/v3suffix/apis/apps/v1"}
            }}"#,
        )
        .expect("the fixture parses");
        let sources = sources_from(index);
        let names: Vec<&str> = sources
            .iter()
            .map(|source| source.group_version.as_str())
            .collect();
        assert_eq!(names, ["apps/v1", "example.com/v1", "v1"]);
        assert!(
            sources
                .iter()
                .all(|source| source.url.starts_with("/openapi/v3")),
            "a URL escaping /openapi/v3 is dropped: {sources:?}"
        );
    }

    #[test]
    fn a_schema_url_may_not_walk_out_of_the_openapi_root() {
        for url in [
            "/openapi/v3",
            "/openapi/v3/api/v1?hash=aaa",
            "/openapi/v3/apis/example.com/v1?hash=bbb",
        ] {
            assert!(under_openapi_root(url), "{url}");
        }
        for url in [
            "/etc/passwd",
            "openapi/v3/api/v1",
            "/openapi/v3suffix/api/v1",
            "/openapi/v3/../../../etc/passwd",
            "/openapi/v3/apis/../../../etc/passwd",
            "/openapi/v3/%2e%2e/%2e%2e/etc/passwd",
            "/openapi/v3/%2E./etc/passwd",
            "/openapi/v3/../secrets?hash=aaa",
        ] {
            assert!(
                !under_openapi_root(url),
                "a prefix test alone would fetch this: {url}"
            );
        }
    }
}
