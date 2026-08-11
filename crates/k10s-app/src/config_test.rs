//! The config files over a real directory, constructed directly rather than
//! from the environment: `from_env` reads `XDG_CONFIG_HOME`/`HOME`, and under
//! `forbid(unsafe_code)` a test cannot set either, so what is pinned here is
//! everything downstream of the paths -- which files are offered to the shell,
//! what a read returns when they are missing, and the theme directory's
//! ordering contract.

use std::path::PathBuf;

use super::*;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("k10s-config-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("a temp directory");
    root
}

fn files_under(root: &PathBuf) -> ConfigFiles {
    ConfigFiles {
        settings: Some(root.join("settings.json")),
        keymap: Some(root.join("keymap.json")),
        themes: Some(root.join("themes")),
    }
}

#[test]
fn the_shell_is_offered_both_paths_or_neither() {
    let root = scratch("paths");
    let paths = files_under(&root).paths().expect("both paths configured");
    assert_eq!(paths.settings, root.join("settings.json"));
    assert_eq!(paths.keymap, root.join("keymap.json"));

    // A platform with no config home offers no settings command at all,
    // rather than a command that opens half of the pair.
    assert!(ConfigFiles::none().paths().is_none());
    let half = ConfigFiles {
        settings: Some(root.join("settings.json")),
        keymap: None,
        themes: None,
    };
    assert!(half.paths().is_none());
}

#[test]
fn missing_files_read_as_empty_rather_than_failing() {
    let root = scratch("missing");
    let text = files_under(&root).read();
    assert_eq!(text.settings, "");
    assert_eq!(text.keymap, "");
    assert!(text.themes.is_empty());
    assert!(ConfigFiles::none().read() == files_under(&root).read());
}

#[test]
fn a_read_carries_what_the_files_say() {
    let root = scratch("content");
    std::fs::write(root.join("settings.json"), "{\"theme\": \"k10s\"}").unwrap();
    std::fs::write(root.join("keymap.json"), "[]").unwrap();
    let text = files_under(&root).read();
    assert_eq!(text.settings, "{\"theme\": \"k10s\"}");
    assert_eq!(text.keymap, "[]");
}

#[test]
fn themes_load_json_files_only_sorted_by_name() {
    let root = scratch("themes");
    let themes = root.join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("zebra.json"), "z").unwrap();
    std::fs::write(themes.join("aurora.json"), "a").unwrap();
    std::fs::write(themes.join("notes.txt"), "not a theme").unwrap();
    std::fs::create_dir_all(themes.join("nested.json")).unwrap();

    let text = files_under(&root).read();
    // Sorted, so two files that both define a theme name resolve the same
    // way on every start; non-json and directories never load.
    assert_eq!(
        text.themes,
        vec![
            ("aurora.json".to_string(), "a".to_string()),
            ("zebra.json".to_string(), "z".to_string()),
        ]
    );
}

#[test]
fn only_a_config_with_some_path_is_worth_polling() {
    let root = scratch("watchable");
    assert!(files_under(&root).watchable());
    assert!(!ConfigFiles::none().watchable());
}
