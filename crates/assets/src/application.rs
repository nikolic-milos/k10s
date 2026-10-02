use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

use crate::icons;

/// The interface face. Six weights, because Zed's UI leans on medium and
/// semibold for emphasis and a synthesised bold is visibly worse.
pub const UI_FONTS: [&str; 6] = [
    "fonts/inter/Inter-Regular.ttf",
    "fonts/inter/Inter-Italic.ttf",
    "fonts/inter/Inter-Medium.ttf",
    "fonts/inter/Inter-SemiBold.ttf",
    "fonts/inter/Inter-SemiBoldItalic.ttf",
    "fonts/inter/Inter-Bold.ttf",
];

/// The buffer and terminal face.
pub const MONO_FONTS: [&str; 5] = [
    "fonts/lilex/Lilex-Regular.ttf",
    "fonts/lilex/Lilex-Italic.ttf",
    "fonts/lilex/Lilex-Medium.ttf",
    "fonts/lilex/Lilex-Bold.ttf",
    "fonts/lilex/Lilex-BoldItalic.ttf",
];

/// The display face, one weight, used where the product says its own name.
/// This file is CFF-flavoured OpenType (`OTTO`) rather than TrueType, which
/// cosmic-text handles -- and which [`register_fonts`] verifies rather than
/// trusts, because a silently unregistered display face is a fallback nobody
/// notices in review.
pub const DISPLAY_FONTS: [&str; 1] = ["fonts/league-spartan/LeagueSpartan-Bold.otf"];

/// The 512 px mark on brand blue: the window icon, and the source of the
/// hicolor PNG set shipped beside it.
pub const APP_ICON: &str = "brand/k10s.png";

/// Wayland has no per-window icon protocol: the compositor matches the
/// window's app id against a desktop entry's basename and takes the icon from
/// there. So this string, `WindowOptions::app_id`, the `StartupWMClass` line
/// and the `k10s.desktop` filename are one value in four places, and a
/// mismatch shows a generic icon with no error anywhere.
pub const APP_ID: &str = "k10s";

macro_rules! embedded {
    ($($path:literal),+ $(,)?) => {
        &[$(($path, include_bytes!(concat!("../assets/", $path)) as &'static [u8])),+]
    };
}

// Fonts and brand assets use path lookup. Glyphs live in `icons` so the painter
// can index them by model ID without scanning this table.
const EMBEDDED: &[(&str, &'static [u8])] = embedded![
    "fonts/inter/Inter-Regular.ttf",
    "fonts/inter/Inter-Italic.ttf",
    "fonts/inter/Inter-Medium.ttf",
    "fonts/inter/Inter-SemiBold.ttf",
    "fonts/inter/Inter-SemiBoldItalic.ttf",
    "fonts/inter/Inter-Bold.ttf",
    "fonts/inter/OFL.txt",
    "fonts/lilex/Lilex-Regular.ttf",
    "fonts/lilex/Lilex-Italic.ttf",
    "fonts/lilex/Lilex-Medium.ttf",
    "fonts/lilex/Lilex-Bold.ttf",
    "fonts/lilex/Lilex-BoldItalic.ttf",
    "fonts/lilex/OFL.txt",
    "fonts/league-spartan/LeagueSpartan-Bold.otf",
    "fonts/league-spartan/OFL.md",
    "brand/k10s.png",
    "brand/mark-light.png",
    "brand/mark-dark.png",
    "brand/logo-light.png",
    "brand/logo-dark.png",
    "linux/icons/k10s-16.png",
    "linux/icons/k10s-24.png",
    "linux/icons/k10s-32.png",
    "linux/icons/k10s-48.png",
    "linux/icons/k10s-64.png",
    "linux/icons/k10s-128.png",
    "linux/icons/k10s-256.png",
    "linux/icons/k10s-512.png",
    "linux/k10s.desktop",
    "linux/install-desktop-entry.sh",
    "linux/README.md",
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = icons::load(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        match EMBEDDED.iter().find(|(name, _)| *name == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => zed_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut names = Vec::new();
        for (name, _) in icons::all().chain(EMBEDDED.iter().copied()) {
            if name.starts_with(path) && !names.iter().any(|entry: &SharedString| entry == name) {
                names.push(SharedString::new_static(name));
            }
        }
        for name in zed_assets::Assets.list(path)? {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// Register the three families and confirm the text system really has them.
///
/// The confirmation is the point. `add_fonts` reports a parse failure, but a
/// family that parses and then does not answer to its name is the failure that
/// actually happens -- and it degrades into a platform fallback that looks
/// almost right, which is the one outcome typography must never do quietly.
pub fn register_fonts(cx: &gpui::App) -> Result<(), String> {
    let source = cx.asset_source();
    let mut bytes = Vec::with_capacity(UI_FONTS.len() + MONO_FONTS.len() + DISPLAY_FONTS.len());
    for path in UI_FONTS
        .iter()
        .chain(MONO_FONTS.iter())
        .chain(DISPLAY_FONTS.iter())
    {
        let font = source
            .load(path)
            .map_err(|error| format!("cannot load embedded font {path}: {error}"))?
            .ok_or_else(|| format!("embedded font {path} is missing from the asset table"))?;
        bytes.push(font);
    }
    cx.text_system()
        .add_fonts(bytes)
        .map_err(|error| format!("cannot register the embedded fonts: {error}"))?;

    // gpui has no `has_family` query, so the confirmation walks the system
    // list. First photon pays it once; a silent platform fallback would cost
    // every frame after that, and look almost right.
    let available = cx.text_system().all_font_names();
    let missing: Vec<&str> = [
        k10s_theme::DEFAULT_UI_FAMILY,
        k10s_theme::DEFAULT_BUFFER_FAMILY,
        k10s_theme::DISPLAY_FAMILY,
    ]
    .into_iter()
    .filter(|family| !available.iter().any(|name| name == family))
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the text system accepted the font files but does not answer to {}; the shell would \
             render on a platform fallback that is not the typeface this theme was measured with",
            missing.join(", ")
        ));
    }
    Ok(())
}

/// The window icon, decoded. X11 is the only platform that takes one -- Wayland
/// matches `WindowOptions::app_id` to a desktop entry instead -- so a failure
/// here is a missing icon, not a broken session, and the caller treats it that
/// way.
pub fn window_icon() -> Result<image::RgbaImage, String> {
    let bytes = EMBEDDED
        .iter()
        .find(|(name, _)| *name == APP_ICON)
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| format!("{APP_ICON} is missing from the asset table"))?;
    image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map(|image| image.into_rgba8())
        .map_err(|error| format!("cannot decode {APP_ICON}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_indexed_icon_loads_by_the_same_key_and_is_listed_once() {
        let names = Assets.list("icons/").expect("lists");
        for (key, bytes) in icons::all() {
            let loaded = Assets.load(key).expect("loads").expect("present");
            assert_eq!(loaded.as_ref(), bytes, "{key}");
            assert_eq!(names.iter().filter(|name| *name == key).count(), 1, "{key}");
        }
        let tools = Assets.list("icons/tools/").expect("lists");
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|name| name.starts_with("icons/tools/")));
    }

    #[test]
    fn every_font_the_registrar_asks_for_is_in_the_table() {
        for path in UI_FONTS
            .iter()
            .chain(MONO_FONTS.iter())
            .chain(DISPLAY_FONTS.iter())
        {
            assert!(
                EMBEDDED.iter().any(|(name, _)| name == path),
                "{path} is registered but not embedded"
            );
        }
        assert!(EMBEDDED.iter().all(|(_, bytes)| !bytes.is_empty()));
    }

    #[test]
    fn the_table_shadows_zed_for_the_faces_we_own_and_defers_for_its_icons() {
        let ours = Assets
            .load("fonts/lilex/Lilex-Regular.ttf")
            .expect("loads")
            .expect("present");
        let theirs = zed_assets::Assets
            .load("fonts/lilex/Lilex-Regular.ttf")
            .expect("loads")
            .expect("present");
        assert_eq!(
            ours.len(),
            EMBEDDED
                .iter()
                .find(|(name, _)| *name == "fonts/lilex/Lilex-Regular.ttf")
                .expect("embedded")
                .1
                .len(),
            "our copy wins even where the upstream path exists"
        );
        assert!(!theirs.is_empty());

        for icon in [
            "icons/generic_minimize.svg",
            "icons/generic_maximize.svg",
            "icons/generic_restore.svg",
            "icons/generic_close.svg",
            "icons/menu.svg",
            "icons/file_tree.svg",
            "icons/terminal_alt.svg",
            "icons/info.svg",
            "icons/star.svg",
            "icons/server.svg",
            "icons/magnifying_glass.svg",
            "icons/box.svg",
            "icons/git_branch.svg",
            "icons/pull_request.svg",
            "icons/tool_hammer.svg",
            "icons/forward_arrow.svg",
            "icons/settings.svg",
        ] {
            assert!(
                Assets.load(icon).expect("loads").is_some(),
                "{icon} must still resolve through the chain"
            );
        }
        assert!(
            Assets.load("nothing/at/all").is_err(),
            "a path neither source has is upstream's error verbatim, not a silent None"
        );
    }

    #[test]
    fn the_window_icon_decodes_to_a_square() {
        let icon = window_icon().expect("the shipped mark decodes");
        assert_eq!(icon.width(), icon.height());
        assert_eq!(icon.width(), 512);
    }

    #[test]
    fn the_desktop_entry_and_the_app_id_are_the_same_name() {
        let entry = Assets
            .load("linux/k10s.desktop")
            .expect("loads")
            .expect("present");
        let entry = String::from_utf8_lossy(&entry);
        assert!(
            entry.contains(&format!("StartupWMClass={APP_ID}")),
            "a desktop entry whose WM class does not match the app id matches nothing: {entry}"
        );
        assert!(entry.contains("Icon=k10s"));

        // The installer is the fourth place that name appears, and the one that
        // decides what the file on disk is called.
        let script = Assets
            .load("linux/install-desktop-entry.sh")
            .expect("loads")
            .expect("present");
        let script = String::from_utf8_lossy(&script);
        assert!(script.contains(&format!("id={APP_ID}")), "{script}");
        assert!(
            script.contains(&format!("entry={APP_ID}.desktop")),
            "the installed basename is what a Wayland compositor matches against"
        );
        assert!(
            !script.contains("/usr/share"),
            "this script installs for one user and must not write outside their data home"
        );
    }

    #[test]
    fn listing_a_directory_finds_ours_first_and_then_zeds() {
        let fonts = Assets.list("fonts/").expect("lists");
        assert!(fonts.iter().any(|name| name.contains("Inter-Regular")));
        assert!(
            fonts.iter().any(|name| name.contains("ibm-plex")),
            "the chain still reaches upstream: {fonts:?}"
        );
        let brand = Assets.list("brand/").expect("lists");
        assert_eq!(brand.len(), 5, "{brand:?}");
    }

    // The shell names these four paths as string literals, the way it names
    // Zed's window-control SVGs: it draws bytes it does not own. Renaming a
    // file here without renaming it in `k10s_shell::ui::brand_mark` and
    // `brand_logo` produces a missing image and no error anywhere, so the
    // literals are pinned on both sides.
    #[test]
    fn the_four_brand_bitmaps_the_shell_draws_resolve_and_decode() {
        for path in [
            "brand/mark-light.png",
            "brand/mark-dark.png",
            "brand/logo-light.png",
            "brand/logo-dark.png",
        ] {
            let bytes = Assets
                .load(path)
                .expect("loads")
                .unwrap_or_else(|| panic!("{path} is missing from the asset table"));
            let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                .unwrap_or_else(|error| panic!("{path} does not decode: {error}"));
            assert_eq!(
                image.width(),
                image.height(),
                "{path} is not square, so every box it is drawn in distorts it"
            );
            assert!(
                image.color().has_alpha(),
                "{path} has no alpha channel; the mark is drawn over theme background"
            );
        }
    }
}
