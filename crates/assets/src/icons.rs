//! Indexed glyphs shared by the map painter and ordinary SVG elements.
//!
//! The paint path takes a dense model ID and borrows static bytes. Asset-path
//! lookup and enumeration belong to the asset source, outside that path.

use gpui::SharedString;
use k10s_core::{KindId, ToolId};

type Glyph = (&'static str, &'static [u8]);

macro_rules! glyph {
    ($file:literal) => {
        (
            concat!("icons/", $file),
            include_bytes!(concat!("../assets/icons/", $file)) as &'static [u8],
        )
    };
}

macro_rules! tool_glyph {
    ($file:literal) => {
        (
            concat!("icons/tools/", $file),
            include_bytes!(concat!("../assets/icons/tools/", $file)) as &'static [u8],
        )
    };
}

const UNKNOWN_GLYPH: Glyph = glyph!("unknown.svg");

static KIND_GLYPHS: &[Glyph] = &[
    glyph!("deploy.svg"),
    glyph!("sts.svg"),
    glyph!("ds.svg"),
    glyph!("job.svg"),
    glyph!("job.svg"),
    glyph!("unknown.svg"),
    glyph!("unknown.svg"),
    glyph!("pvc.svg"),
    glyph!("svc.svg"),
    glyph!("cm.svg"),
    glyph!("secret.svg"),
    glyph!("svc.svg"),
    glyph!("unknown.svg"),
];

const _: () = assert!(
    KIND_GLYPHS.len() == k10s_core::BUILTIN_KIND_COUNT as usize,
    "every built-in kind needs a glyph"
);

static TOOL_GLYPHS: &[Glyph] = &[
    UNKNOWN_GLYPH,
    tool_glyph!("apacheairflow.svg"),
    tool_glyph!("argo.svg"),
    tool_glyph!("apachecassandra.svg"),
    tool_glyph!("cilium.svg"),
    tool_glyph!("clickhouse.svg"),
    tool_glyph!("consul.svg"),
    tool_glyph!("elasticsearch.svg"),
    tool_glyph!("envoyproxy.svg"),
    tool_glyph!("etcd.svg"),
    tool_glyph!("falco.svg"),
    tool_glyph!("fluentbit.svg"),
    tool_glyph!("fluentd.svg"),
    tool_glyph!("flux.svg"),
    tool_glyph!("grafana.svg"),
    tool_glyph!("harbor.svg"),
    tool_glyph!("headlamp.svg"),
    tool_glyph!("istio.svg"),
    tool_glyph!("jaeger.svg"),
    tool_glyph!("jenkins.svg"),
    tool_glyph!("apachekafka.svg"),
    tool_glyph!("kargo.svg"),
    tool_glyph!("keycloak.svg"),
    tool_glyph!("kibana.svg"),
    tool_glyph!("kubernetes.svg"),
    tool_glyph!("kyverno.svg"),
    tool_glyph!("loki.svg"),
    tool_glyph!("mariadb.svg"),
    tool_glyph!("minio.svg"),
    tool_glyph!("mongodb.svg"),
    tool_glyph!("mysql.svg"),
    tool_glyph!("natsdotio.svg"),
    tool_glyph!("nginx.svg"),
    tool_glyph!("openbao.svg"),
    tool_glyph!("opentelemetry.svg"),
    tool_glyph!("postgresql.svg"),
    tool_glyph!("prometheus.svg"),
    tool_glyph!("rabbitmq.svg"),
    tool_glyph!("redis.svg"),
    tool_glyph!("temporal.svg"),
    tool_glyph!("tetragon.svg"),
    tool_glyph!("traefikproxy.svg"),
    tool_glyph!("vault.svg"),
    tool_glyph!("velero.svg"),
];

const _: () = assert!(
    TOOL_GLYPHS.len() == k10s_core::BUILTIN_TOOL_COUNT as usize,
    "every built-in vendor needs a glyph"
);

fn glyph_of(table: &'static [Glyph], idx: usize) -> (SharedString, &'static [u8]) {
    let (key, data) = table.get(idx).copied().unwrap_or(UNKNOWN_GLYPH);
    (SharedString::new_static(key), data)
}

#[cfg(feature = "application")]
pub(crate) fn load(path: &str) -> Option<&'static [u8]> {
    all().find(|(key, _)| *key == path).map(|(_, data)| data)
}

/// The built-in kind glyph, with a fallback for discovered kinds.
pub fn kind_icon(kind: KindId) -> (SharedString, &'static [u8]) {
    glyph_of(KIND_GLYPHS, kind.0 as usize)
}

/// The catalog tool glyph, with the same fallback for unknown tools.
pub fn tool_icon(tool: ToolId) -> (SharedString, &'static [u8]) {
    glyph_of(TOOL_GLYPHS, tool.0 as usize)
}

#[cfg(feature = "application")]
pub(crate) fn all() -> impl Iterator<Item = Glyph> {
    KIND_GLYPHS.iter().chain(TOOL_GLYPHS).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_past_either_glyph_table_falls_back_to_the_unknown_glyph() {
        let (unknown_key, _) = UNKNOWN_GLYPH;
        assert_eq!(glyph_of(KIND_GLYPHS, usize::MAX).0.as_ref(), unknown_key);
        assert_eq!(glyph_of(TOOL_GLYPHS, 9_001).0.as_ref(), unknown_key);
        assert_eq!(
            glyph_of(TOOL_GLYPHS, 0).0.as_ref(),
            unknown_key,
            "tool zero is the catalog's own unknown"
        );
        assert_ne!(glyph_of(KIND_GLYPHS, 0).0.as_ref(), unknown_key);
    }
}
