use gpui::Rgba;
use k10s_core::{BUILTIN_TOOL_COUNT, KindId, Severity, ToolId};

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

pub fn pod_color(h: Severity) -> Rgba {
    match h {
        Severity::Ok => gpui::rgb(0x2ea043),
        Severity::Warn => gpui::rgb(0xd29922),
        Severity::Err => gpui::rgb(0xf85149),
        Severity::Unknown => gpui::rgb(0x545d68),
    }
}

pub fn workload_colors(h: Severity) -> (Rgba, Rgba) {
    match h {
        Severity::Ok => (gpui::rgb(0x1d132f), gpui::rgb(0x7a5fb5).alpha(0.55)),
        Severity::Warn => (gpui::rgb(0x2a2312), gpui::rgb(0xd29922).alpha(0.75)),
        Severity::Err => (gpui::rgb(0x2d1417), gpui::rgb(0xf85149).alpha(0.85)),
        Severity::Unknown => (gpui::rgb(0x191325), gpui::rgb(0x545d68).alpha(0.6)),
    }
}

pub const UNKNOWN_KIND: u32 = 0x9e96b8;

static KIND_COLORS: &[u32] = &[
    0x7a5fb5, 0x9a7fd0, 0x6f8fd0, 0x8fd06f, 0xd0b06f, 0x2ea043, 0x342552, 0xe36bdc, 0x3fd68f,
    0x6aa9ff, 0xd8a63a, 0x4fd0c0, 0xa79fc2,
];

const _: () = assert!(
    KIND_COLORS.len() == k10s_core::BUILTIN_KIND_COUNT as usize,
    "every built-in kind needs a colour"
);

pub fn kind_color(kind: KindId) -> Rgba {
    gpui::rgb(
        KIND_COLORS
            .get(kind.0 as usize)
            .copied()
            .unwrap_or(UNKNOWN_KIND),
    )
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

static TOOL_COLORS: &[u32] = &[
    UNKNOWN_KIND,
    0x017CEE,
    0xEF7B4D,
    0x1287B1,
    0xFFCC01,
    0xF24C53,
    0x8CB3BF,
    0xAC6199,
    0x419EDA,
    0x49BDA5,
    0x0E83C8,
    0x5468FF,
    0xF46800,
    0x60B932,
    0x466BB0,
    0x66CFE3,
    0xD24939,
    0x9C9A9B,
    0xAFAFAF,
    0x8CB3BF,
    0x326CE5,
    0x8CA4AB,
    0xC72E49,
    0x47A248,
    0x4479A1,
    0x27AAE1,
    0x009639,
    0x8C8C8C,
    0x4169E1,
    0xE6522C,
    0xFF6600,
    0xFF4438,
    0x8C8C8C,
    0x24A1C1,
    0xFFEC6E,
];

const _: () = assert!(
    TOOL_COLORS.len() == BUILTIN_TOOL_COUNT as usize,
    "every built-in vendor needs a colour"
);

pub fn tool_color(tool: ToolId) -> Rgba {
    gpui::rgb(
        TOOL_COLORS
            .get(tool.0 as usize)
            .copied()
            .unwrap_or(UNKNOWN_KIND),
    )
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
