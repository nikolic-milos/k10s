use gpui::Rgba;
use k10s_core::{Health, SatKind, Tool};

pub const BG: u32 = 0x160f26;
pub const HEX_LINE: u32 = 0x4d3a78;
pub const NS_FILL: u32 = 0x1e1533;
pub const NS_BORDER: u32 = 0x342552;
pub const EDGE: u32 = 0x58a6ff;
pub const HUD_BG: u32 = 0x120c1f;
pub const HUD_TEXT: u32 = 0xa79fc2;

pub const CURVE_CORE: u32 = 0xf0d5ee;
pub const CURVE_CORE_ALPHA: f32 = 0.72;
pub const CURVE_GLOW: u32 = 0xc45ecb;
pub const CURVE_GLOW_ALPHA: f32 = 0.16;

pub const CARD_HEADER_FILL: u32 = 0x2a1c49;

pub fn pod_color(h: Health) -> Rgba {
    match h {
        Health::Ok => gpui::rgb(0x2ea043),
        Health::Warn => gpui::rgb(0xd29922),
        Health::Err => gpui::rgb(0xf85149),
        Health::Unknown => gpui::rgb(0x545d68),
    }
}

pub fn workload_colors(h: Health) -> (Rgba, Rgba) {
    match h {
        Health::Ok => (gpui::rgb(0x1d132f), gpui::rgb(0x7a5fb5).alpha(0.55)),
        Health::Warn => (gpui::rgb(0x2a2312), gpui::rgb(0xd29922).alpha(0.75)),
        Health::Err => (gpui::rgb(0x2d1417), gpui::rgb(0xf85149).alpha(0.85)),
        Health::Unknown => (gpui::rgb(0x191325), gpui::rgb(0x545d68).alpha(0.6)),
    }
}

pub fn sat_color(kind: SatKind) -> Rgba {
    match kind {
        SatKind::Volume => gpui::rgb(0xe36bdc),
        SatKind::Service => gpui::rgb(0x3fd68f),
        SatKind::ConfigMap => gpui::rgb(0x6aa9ff),
        SatKind::Secret => gpui::rgb(0xd8a63a),
    }
}

pub fn heat_color(frac: f32) -> Rgba {
    let t = (frac * 2.5).clamp(0.0, 1.0);
    lerp(
        lerp_c(0x18342a, 0x7a5a12, (t * 2.0).min(1.0)),
        0x6e1a1e,
        (t * 2.0 - 1.0).max(0.0),
    )
}

pub fn heat_border(frac: f32) -> Rgba {
    let t = (frac * 4.0).clamp(0.0, 1.0);
    lerp(
        Rgba {
            r: 0.20,
            g: 0.15,
            b: 0.32,
            a: 1.0,
        },
        0xb62324,
        t * 0.8,
    )
}

pub fn tool_color(tool: Tool) -> Rgba {
    let hex = match tool {
        Tool::Airflow => 0x017CEE,
        Tool::ArgoCd => 0xEF7B4D,
        Tool::Cassandra => 0x1287B1,
        Tool::ClickHouse => 0xFFCC01,
        Tool::Consul => 0xF24C53,
        Tool::Elasticsearch => 0x8CB3BF,
        Tool::Envoy => 0xAC6199,
        Tool::Etcd => 0x419EDA,
        Tool::FluentBit => 0x49BDA5,
        Tool::Fluentd => 0x0E83C8,
        Tool::Flux => 0x5468FF,
        Tool::Grafana => 0xF46800,
        Tool::Harbor => 0x60B932,
        Tool::Istio => 0x466BB0,
        Tool::Jaeger => 0x66CFE3,
        Tool::Jenkins => 0xD24939,
        Tool::Kafka => 0x9C9A9B,
        Tool::Keycloak => 0xAFAFAF,
        Tool::Kibana => 0x8CB3BF,
        Tool::Kubernetes => 0x326CE5,
        Tool::MariaDb => 0x8CA4AB,
        Tool::Minio => 0xC72E49,
        Tool::MongoDb => 0x47A248,
        Tool::MySql => 0x4479A1,
        Tool::Nats => 0x27AAE1,
        Tool::Nginx => 0x009639,
        Tool::OpenTelemetry => 0x8C8C8C,
        Tool::Postgres => 0x4169E1,
        Tool::Prometheus => 0xE6522C,
        Tool::RabbitMq => 0xFF6600,
        Tool::Redis => 0xFF4438,
        Tool::Temporal => 0x8C8C8C,
        Tool::Traefik => 0x24A1C1,
        Tool::Vault => 0xFFEC6E,
        Tool::None => 0x9E96B8,
    };
    gpui::rgb(hex)
}

pub fn scale_alpha(mut c: Rgba, a: f32) -> Rgba {
    c.a *= a;
    c
}

pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

fn channel(c: u32, shift: u32) -> f32 {
    ((c >> shift) & 0xff) as f32 / 255.0
}

fn lerp_c(a: u32, b: u32, t: f32) -> Rgba {
    lerp(
        Rgba {
            r: channel(a, 16),
            g: channel(a, 8),
            b: channel(a, 0),
            a: 1.0,
        },
        b,
        t,
    )
}

fn lerp(a: Rgba, b: u32, t: f32) -> Rgba {
    Rgba {
        r: a.r + (channel(b, 16) - a.r) * t,
        g: a.g + (channel(b, 8) - a.g) * t,
        b: a.b + (channel(b, 0) - a.b) * t,
        a: 1.0,
    }
}
