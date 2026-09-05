//! Saved Starmap views: camera + filters + overlay, a local file, never a CRD.
//!
//! The file is JSON with comments allowed the same way settings are. A view
//! that names a secret, a token, or a snapshot uid is refused: those are not
//! camera state. Fly-to already honours reduce-motion; loading a view must
//! go through that same motion, not teleport, unless reduce-motion is on.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct MapFilter {
    pub namespaces: Vec<String>,
    pub kinds: Vec<String>,
    pub label_selector: Vec<(String, String)>,
    pub health: Option<HealthFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthFilter {
    Unhealthy,
    Healthy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraPose {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedView {
    pub name: String,
    pub camera: CameraPose,
    pub filter: MapFilter,
    pub overlay: Option<String>,
    pub layout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    NotJson(String),
    ForbiddenField(&'static str),
    MissingName,
    UnknownLayout(String),
}

impl std::fmt::Display for ViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewError::NotJson(why) => write!(f, "saved view did not parse: {why}"),
            ViewError::ForbiddenField(field) => {
                write!(f, "saved view must not carry {field}")
            }
            ViewError::MissingName => write!(f, "saved view has no name"),
            ViewError::UnknownLayout(name) => {
                write!(f, "saved view layout must be spread or dense, not {name}")
            }
        }
    }
}

const FORBIDDEN: &[&str] = &["secret", "token", "password", "kubeconfig", "snapshot"];

pub fn parse_view(text: &str) -> Result<SavedView, ViewError> {
    let stripped = strip_jsonc(text);
    let value: Value =
        serde_json::from_str(&stripped).map_err(|error| ViewError::NotJson(error.to_string()))?;
    refuse_forbidden(&value)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ViewError::MissingName)?
        .to_string();
    let camera = value.get("camera").unwrap_or(&Value::Null);
    let layout = value
        .get("layout")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(name) = layout.as_deref()
        && !matches!(name, "spread" | "dense")
    {
        return Err(ViewError::UnknownLayout(name.to_string()));
    }
    Ok(SavedView {
        name,
        camera: CameraPose {
            x: number(camera, "x"),
            y: number(camera, "y"),
            zoom: number(camera, "zoom").max(0.001),
        },
        filter: read_filter(value.get("filter").unwrap_or(&Value::Null)),
        overlay: value
            .get("overlay")
            .and_then(Value::as_str)
            .map(str::to_string),
        layout,
    })
}

impl SavedView {
    pub fn overlay_kind(&self) -> Option<k10s_map::OverlayKind> {
        self.overlay
            .as_deref()
            .and_then(k10s_map::OverlayKind::parse)
    }

    /// The camera the map already flies to. Clamped so a saved zoom of 0
    /// cannot be divided by on the way there.
    pub fn camera_target(&self) -> k10s_atlas::Camera {
        k10s_atlas::Camera {
            cx: self.camera.x,
            cy: self.camera.y,
            zoom: self.camera.zoom,
        }
        .clamped()
    }

    /// What loading this view actually does. Overlay and camera apply;
    /// filter and layout are kept on the file so a later apply has
    /// somewhere to read them, and named here so the status line does
    /// not pretend they already changed the scene.
    pub fn load_status(&self) -> String {
        let mut dropped = Vec::new();
        if self.filter.is_constrained() {
            dropped.push("filter");
        }
        if self.layout.is_some() {
            dropped.push("layout");
        }
        if dropped.is_empty() {
            format!("loaded view {}", self.name)
        } else {
            format!(
                "loaded view {} ({} not applied)",
                self.name,
                dropped.join(" and ")
            )
        }
    }
}

impl MapFilter {
    pub fn is_constrained(&self) -> bool {
        !self.namespaces.is_empty()
            || !self.kinds.is_empty()
            || !self.label_selector.is_empty()
            || self.health.is_some()
    }
}

fn read_filter(value: &Value) -> MapFilter {
    MapFilter {
        namespaces: strings(value.get("namespaces")),
        kinds: strings(value.get("kinds")),
        label_selector: pairs(value.get("labels")),
        health: match value.get("health").and_then(Value::as_str) {
            Some("unhealthy") => Some(HealthFilter::Unhealthy),
            Some("healthy") => Some(HealthFilter::Healthy),
            _ => None,
        },
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

fn number(value: &Value, key: &str) -> f32 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0) as f32
}

fn refuse_forbidden(value: &Value) -> Result<(), ViewError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                if FORBIDDEN.iter().any(|f| lower.contains(f)) {
                    return Err(ViewError::ForbiddenField("a secret, token, or snapshot"));
                }
                refuse_forbidden(child)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                refuse_forbidden(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn strip_jsonc(text: &str) -> String {
    k10s_theme::strip_jsonc(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_round_trips_camera_and_filters() {
        let view = parse_view(
            r#"{
              "name": "prod errors",
              "camera": {"x": 10, "y": 20, "zoom": 4},
              "filter": {"namespaces": ["prod"], "health": "unhealthy"},
              "overlay": "policy",
              "layout": "dense"
            }"#,
        )
        .unwrap();
        assert_eq!(view.name, "prod errors");
        assert_eq!(view.camera.zoom, 4.0);
        assert_eq!(view.filter.namespaces, ["prod"]);
        assert_eq!(view.filter.health, Some(HealthFilter::Unhealthy));
        assert_eq!(view.overlay.as_deref(), Some("policy"));
        assert_eq!(view.overlay_kind(), Some(k10s_map::OverlayKind::Policy));
        assert_eq!(view.layout.as_deref(), Some("dense"));
        let camera = view.camera_target();
        assert_eq!(camera.cx, 10.0);
        assert_eq!(camera.cy, 20.0);
        assert_eq!(camera.zoom, 4.0);
        assert_eq!(
            view.load_status(),
            "loaded view prod errors (filter and layout not applied)"
        );
    }

    #[test]
    fn an_unknown_layout_is_refused() {
        match parse_view(r#"{"name":"x","layout":"by-node"}"#) {
            Err(ViewError::UnknownLayout(name)) => assert_eq!(name, "by-node"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_camera_only_view_does_not_claim_a_dropped_axis() {
        let view = parse_view(r#"{"name":"home","camera":{"x":1,"y":2,"zoom":3}}"#).unwrap();
        assert_eq!(view.load_status(), "loaded view home");
    }

    #[test]
    fn an_unknown_overlay_name_is_kept_but_does_not_select_a_kind() {
        let view = parse_view(r#"{"name":"x","overlay":"grafana"}"#).unwrap();
        assert_eq!(view.overlay.as_deref(), Some("grafana"));
        assert_eq!(view.overlay_kind(), None);
    }

    #[test]
    fn a_zero_zoom_is_clamped_so_fly_to_can_divide() {
        let view = parse_view(r#"{"name":"x","camera":{"zoom":0}}"#).unwrap();
        let camera = view.camera_target();
        assert!(
            camera.zoom.is_finite() && camera.zoom > 0.0,
            "fly-to divides by zoom: {camera:?}"
        );
    }

    #[test]
    fn a_token_field_is_refused() {
        match parse_view(r#"{"name":"x","token":"secret"}"#) {
            Err(ViewError::ForbiddenField(_)) => {}
            other => panic!("{other:?}"),
        }
    }
}
