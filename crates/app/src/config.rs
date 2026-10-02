//! The user's configuration, from file to gpui global.
//!
//! Three jobs, one lifecycle: discover the settings, keymap and theme files
//! under the config home; poll their content every two seconds on the
//! background executor (no watcher dependency, and never under `--bench`);
//! and publish what they say -- settings, theme resolved against the window's
//! current appearance, and key bindings with their derived suppressors -- as
//! the app globals every view reads.
//!
//! A file that is not there is an answer: it means defaults. A file that is
//! there and will not read is not, so a poll that hits one keeps whatever is
//! already applied rather than publishing an empty document over it.

#[derive(Clone)]
pub(crate) struct ConfigFiles {
    settings: Option<std::path::PathBuf>,
    keymap: Option<std::path::PathBuf>,
    themes: Option<std::path::PathBuf>,
}

#[derive(PartialEq)]
pub(crate) struct ConfigText {
    settings: String,
    keymap: String,
    // Every `themes/*.json`, by file name so a note can say which file it came
    // from, and sorted so two files that both define a name resolve the same
    // way on every start.
    themes: Vec<(String, String)>,
    // The files that exist and would not read. A missing file is an answer --
    // it means defaults -- but a permission error is not, and applying it as an
    // empty document would silently reset a running session's theme and keymap.
    unreadable: Vec<String>,
}

impl ConfigFiles {
    pub(crate) fn none() -> ConfigFiles {
        ConfigFiles {
            settings: None,
            keymap: None,
            themes: None,
        }
    }

    pub(crate) fn from_env() -> ConfigFiles {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| std::path::PathBuf::from(home).join(".config"))
            })
            .map(|path| path.join("k10s"));
        match base {
            Some(dir) => ConfigFiles {
                settings: Some(dir.join("settings.json")),
                keymap: Some(dir.join("keymap.json")),
                themes: Some(dir.join("themes")),
            },
            None => ConfigFiles::none(),
        }
    }

    // The shell needs both paths present to offer the settings and keymap
    // commands; a platform with no config home offers neither.
    pub(crate) fn paths(&self) -> Option<k10s_shell::ConfigPaths> {
        Some(k10s_shell::ConfigPaths {
            settings: self.settings.clone()?,
            keymap: self.keymap.clone()?,
        })
    }

    pub(crate) fn read(&self) -> ConfigText {
        let mut unreadable = Vec::new();
        let mut read = |path: &Option<std::path::PathBuf>| match path {
            Some(path) => read_text(path, &mut unreadable).unwrap_or_default(),
            None => String::new(),
        };
        let settings = read(&self.settings);
        let keymap = read(&self.keymap);
        let themes = self.read_themes(&mut unreadable);
        ConfigText {
            settings,
            keymap,
            themes,
            unreadable,
        }
    }

    fn read_themes(&self, unreadable: &mut Vec<String>) -> Vec<(String, String)> {
        let Some(dir) = &self.themes else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                note_unreadable(dir, &error, unreadable);
                return Vec::new();
            }
        };
        let mut files: Vec<(String, String)> = entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "json")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            })
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                read_text(&entry.path(), unreadable).map(|text| (name, text))
            })
            .collect();
        files.sort();
        files
    }

    fn watchable(&self) -> bool {
        self.settings.is_some() || self.keymap.is_some() || self.themes.is_some()
    }
}

// A file that is not there means defaults; anything else that stops a read is
// reported, so the poller can keep what it already applied.
fn read_text(path: &std::path::Path, unreadable: &mut Vec<String>) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            note_unreadable(path, &error, unreadable);
            None
        }
    }
}

fn note_unreadable(path: &std::path::Path, error: &std::io::Error, unreadable: &mut Vec<String>) {
    if error.kind() != std::io::ErrorKind::NotFound {
        unreadable.push(format!("{}: {error}", path.display()));
    }
}

// The window's light/dark appearance, held at app scope because k10s opens
// exactly one window and the theme is published as an app global. The window
// observer writes it on every `appearance_changed`; nothing samples it once at
// startup, because a desktop that switches at sunset must move the editor
// with it.
pub(crate) struct DesktopAppearance(pub(crate) k10s_theme::Appearance);

impl gpui::Global for DesktopAppearance {}

fn appearance(cx: &gpui::App) -> k10s_theme::Appearance {
    cx.try_global::<DesktopAppearance>()
        .map(|current| current.0)
        .unwrap_or_default()
}

pub(crate) fn apply_config(text: &ConfigText, cx: &mut gpui::App) {
    for path in &text.unreadable {
        eprintln!("k10s: cannot read {path}; its defaults are used instead");
    }
    let mut registry = k10s_theme::ThemeRegistry::builtin();
    for (file, body) in &text.themes {
        let loaded = k10s_theme::parse_family(body);
        for note in &loaded.notes {
            eprintln!("k10s: themes/{file}: {note}");
        }
        if let Some(family) = loaded.family {
            registry.add_family(family);
        }
    }

    let mut loaded = k10s_shell::settings::parse(&text.settings);
    // The families the text system really has, asked after registration: an
    // unknown family must be a note, never gpui's silent platform fallback.
    let available = cx.text_system().all_font_names();
    loaded.notes.extend(k10s_shell::settings::resolve_families(
        &mut loaded.settings,
        &available,
    ));
    for note in &loaded.notes {
        eprintln!("k10s: {note}");
    }

    cx.set_global(k10s_theme::ActiveRegistry(std::sync::Arc::new(registry)));
    cx.set_global(k10s_theme::ActiveTypography(
        loaded.settings.typography.clone(),
    ));
    // Onto gpui itself, not only into our own global: gpui refreshes every
    // window when it changes and its own animation elements consult it, so
    // telling it once is what makes the setting mean the same thing everywhere.
    cx.set_reduce_motion(loaded.settings.reduce_motion);
    cx.set_global(k10s_shell::settings::ActiveSettings(loaded.settings));
    publish_theme(cx);

    let (parsed, notes) = k10s_shell::keymap::parse_keymap(&text.keymap);
    for note in &notes {
        eprintln!("k10s: {note}");
    }
    let (user_bindings, notes) = k10s_shell::keymap::build(&parsed, cx);
    for note in &notes {
        eprintln!("k10s: {note}");
    }
    let defaults = k10s_shell::keybindings();
    let input_suppressors =
        k10s_shell::input_suppressors(defaults.iter().chain(user_bindings.iter()));
    cx.clear_key_bindings();
    cx.bind_keys(defaults);
    // Bound after the defaults, so the user's file wins ties.
    cx.bind_keys(user_bindings);
    // Deeper input contexts must still capture any new plain Workspace key
    // introduced by the user's file. Explicit Palette/Typing/Terminal
    // bindings are detected above and remain authoritative.
    cx.bind_keys(input_suppressors);
}

// Resolve the settings' theme selection against the appearance the window is
// showing right now, patch it with any overrides, and publish it. Called on
// every settings reload and on every appearance change, because both of those
// can change the answer without changing the other.
pub(crate) fn publish_theme(cx: &mut gpui::App) {
    let settings = k10s_shell::settings::active(cx).clone();
    let registry = k10s_theme::registry(cx).clone();
    let appearance = appearance(cx);
    let name = settings.theme.name(appearance);
    let theme = match registry.get(name) {
        Some(theme) => theme.clone(),
        None => {
            let known: Vec<String> = registry
                .names()
                .into_iter()
                .map(|name| name.to_string())
                .collect();
            eprintln!(
                "k10s: settings name an unknown theme {name:?}; themes: {}",
                known.join(", ")
            );
            registry.default_for(appearance).clone()
        }
    };
    let theme = if settings.theme_overrides.is_empty() {
        theme
    } else {
        let mut patched = (*theme).clone();
        settings.theme_overrides.apply(&mut patched);
        std::sync::Arc::new(patched)
    };
    cx.set_global(k10s_theme::ActiveTheme(theme));
}

pub(crate) fn watch_config(config: ConfigFiles, mut last: ConfigText, cx: &mut gpui::App) {
    if !config.watchable() {
        return;
    }
    let background = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let mut warned = false;
        loop {
            background.timer(std::time::Duration::from_secs(2)).await;
            // File I/O never belongs on GPUI's foreground executor. Reading
            // two tiny files is cheap, but a stalled network-mounted home
            // directory otherwise turns a harmless settings poll into a
            // visible frame hitch.
            let reader = config.clone();
            let now = background.spawn(async move { reader.read() }).await;
            // A file that stopped being readable is not a file that was
            // deleted. Reloading it as an empty document would reset the theme,
            // the keymap and reduced motion under a session that changed
            // nothing, so what is already applied stays applied and the reason
            // is said once rather than every two seconds.
            if !now.unreadable.is_empty() {
                if !warned {
                    for path in &now.unreadable {
                        eprintln!(
                            "k10s: cannot read {path}; keeping the configuration already loaded"
                        );
                    }
                    warned = true;
                }
                continue;
            }
            warned = false;
            if now != last {
                cx.update(|cx| {
                    apply_config(&now, cx);
                    cx.refresh_windows();
                });
                last = now;
            }
        }
    })
    .detach();
}

#[cfg(test)]
#[path = "config_test.rs"]
mod tests;
