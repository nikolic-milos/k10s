//! What a person can write and still get a working theme: six lines, colours
//! in any of the four shapes people spell them, comments and trailing commas.
//! Every wrong thing is a note against the value it inherited, never a refusal
//! to load.

use super::*;

#[test]
fn six_lines_are_a_working_theme() {
    let loaded = parse_family(
        r##"{
              "name": "Midnight",
              "author": "someone",
              "themes": [
                { "name": "Midnight", "appearance": "dark",
                  "style": { "editor.background": "#101014" } }
              ]
            }"##,
    );
    assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    let family = loaded.family.expect("a family");
    assert_eq!(family.name.as_ref(), "Midnight");
    assert_eq!(family.author.as_ref(), "someone");
    let theme = &family.themes[0];
    assert_eq!(theme.shell.editor_background, 0x101014);
    assert_eq!(
        theme.shell.text, K10S_DARK.shell.text,
        "an absent key inherits the built-in of the same appearance"
    );
    assert_eq!(theme.map.bg, K10S_DARK.map.bg);
}

#[test]
fn a_light_variant_inherits_the_light_built_in() {
    let loaded = parse_family(r#"{ "themes": [ { "name": "Paper", "appearance": "light" } ] }"#);
    assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    let family = loaded.family.expect("a family");
    assert_eq!(family.themes[0].appearance, Appearance::Light);
    assert_eq!(family.themes[0].shell.editor_background, 0xffffff);
    assert_eq!(
        family.name.as_ref(),
        "user",
        "an unnamed family still loads"
    );
}

#[test]
fn every_wrong_thing_is_a_note_and_the_value_is_the_one_it_inherited() {
    let loaded = parse_family(
        r##"{
              "themes": [
                { "name": "Broken", "style": {
                    "editor_background": "not a colour",
                    "nonsense": "#ffffff",
                    "syntax": { "string": "#zzz", "invented": "#ffffff" },
                    "map": { "kind_colors": ["#ffffff"] }
                } }
              ]
            }"##,
    );
    let family = loaded.family.expect("a broken theme still loads");
    let theme = &family.themes[0];
    assert_eq!(
        theme.shell.editor_background,
        K10S_DARK.shell.editor_background
    );
    assert_eq!(theme.syntax.string, K10S_DARK.syntax.string);
    assert_eq!(theme.map.kind_colors, K10S_DARK.map.kind_colors);
    assert_eq!(loaded.notes.len(), 5, "{:?}", loaded.notes);
    assert!(loaded.notes.iter().all(|note| note.contains("Broken")));
    assert!(
        loaded
            .notes
            .iter()
            .any(|note| note.contains("not a colour"))
    );
    assert!(loaded.notes.iter().any(|note| note.contains("nonsense")));
    assert!(loaded.notes.iter().any(|note| note.contains("invented")));
    assert!(
        loaded
            .notes
            .iter()
            .any(|note| note.contains("needs exactly 13 colours")),
        "{:?}",
        loaded.notes
    );
}

#[test]
fn a_ramp_is_all_or_nothing() {
    let loaded = parse_family(
        r##"{ "themes": [ { "name": "Half", "style": { "map": {
                 "heat_fill": ["#111111", "not a colour", "#333333"] } } } ] }"##,
    );
    let theme = &loaded.family.expect("a family").themes[0];
    assert_eq!(
        theme.map.heat_fill, K10S_DARK.map.heat_fill,
        "one bad stop leaves the inherited ramp rather than a half-applied one"
    );
    assert_eq!(loaded.notes.len(), 1, "{:?}", loaded.notes);
}

#[test]
fn a_file_that_is_not_a_theme_file_says_so_once() {
    for (text, expected) in [
        ("{ not json", "not valid JSON"),
        ("[1, 2]", "must be an object"),
        (r#"{"themes": []}"#, "declares no usable themes"),
        (r#"{"themes": [{}]}"#, "cannot be selected"),
    ] {
        let loaded = parse_family(text);
        assert!(loaded.family.is_none(), "{text}");
        assert!(
            loaded.notes.iter().any(|note| note.contains(expected)),
            "{text} -> {:?}",
            loaded.notes
        );
    }
    assert_eq!(parse_family("   ").notes, Vec::<String>::new());
}

#[test]
fn zed_writes_syntax_tokens_as_objects_and_people_write_them_as_strings() {
    let loaded = parse_family(
        r##"{ "themes": [ { "name": "Both", "style": { "syntax": {
                 "string": { "color": "#112233", "font_style": "italic" },
                 "comment": "#445566" } } } ] }"##,
    );
    let theme = &loaded.family.expect("a family").themes[0];
    assert_eq!(theme.syntax.string, 0x112233);
    assert_eq!(theme.syntax.comment, 0x445566);
    assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
}

#[test]
fn colours_come_in_the_four_shapes_people_write() {
    assert_eq!(parse_color("#abc"), Some((0xaabbcc, 1.0)));
    assert_eq!(parse_color("1800ad"), Some((0x1800ad, 1.0)));
    assert_eq!(parse_color("#1800adff"), Some((0x1800ad, 1.0)));
    let (rgb, alpha) = parse_color("#1800ad80").expect("eight digits carry alpha");
    assert_eq!(rgb, 0x1800ad);
    assert!((alpha - 0.502).abs() < 0.01, "{alpha}");
    for bad in ["", "#12345", "#gggggg", "rebeccapurple"] {
        assert_eq!(parse_color(bad), None, "{bad}");
    }
}

#[test]
fn an_alpha_on_a_key_that_is_painted_opaque_is_applied_and_said() {
    // Eight-digit colours are what a person copies out of a Zed theme. The hue
    // is kept, because refusing it would be worse, but a file that asks for
    // translucency on a key that has none must not look like it got it.
    let loaded = parse_family(
        r##"{ "name": "Alpha", "themes": [ { "name": "Alpha", "style": {
             "text": "#ff000080",
             "background": "#101010ff",
             "map": { "heat_fill": ["#11223344", "#556677", "#8899aa"] }
           } } ] }"##,
    );
    let family = loaded.family.expect("a family");
    let theme = &family.themes[0];
    assert_eq!(theme.shell.text, 0xff0000, "the colour still lands");
    assert_eq!(theme.shell.background, 0x101010);
    assert_eq!(
        theme.map.heat_fill,
        [0x112233, 0x556677, 0x8899aa],
        "a ramp with an alpha in it still lands whole"
    );

    let notes: Vec<&String> = loaded
        .notes
        .iter()
        .filter(|note| note.contains("opaque"))
        .collect();
    assert_eq!(notes.len(), 2, "{:?}", loaded.notes);
    assert!(notes[0].contains("text"), "{:?}", notes[0]);
    assert!(notes[1].contains("heat_fill"), "{:?}", notes[1]);
    assert!(
        !loaded.notes.iter().any(|note| note.contains("background")),
        "a fully opaque eight-digit colour is exactly what it says: {:?}",
        loaded.notes
    );
}

#[test]
fn overrides_complain_once_and_then_replay_silently() {
    let (overrides, notes) = parse_overrides(&serde_json::json!({
        "editor_background": "#101014",
        "invented": "#ffffff"
    }));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("invented"));

    let mut theme = crate::K10S_LIGHT.clone();
    overrides.apply(&mut theme);
    assert_eq!(theme.shell.editor_background, 0x101014);
    assert_eq!(
        theme.shell.text, 0x000000,
        "an override patches what it names and nothing else"
    );
    assert!(!overrides.is_empty());

    let (_, notes) = parse_overrides(&serde_json::json!("nope"));
    assert!(notes[0].contains("must be an object"), "{notes:?}");
}

#[test]
fn comments_and_trailing_commas_are_what_a_person_writes() {
    let loaded = parse_family(
        r##"// my theme
            {
              "themes": [
                { "name": "Commented", "style": { "text": "#ffffff" }, },
              ],
            }"##,
    );
    assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    assert_eq!(
        loaded.family.expect("a family").themes[0].shell.text,
        0xffffff
    );

    // Block comments, a comment sitting between a trailing comma and the brace
    // that closes the thing it trails, and a `//` inside a string that is not a
    // comment at all.
    let dense = parse_family(
        r##"/* the whole file
               is explained up here */
            {
              "name": "Dense",
              "themes": [
                {
                  "name": "https://example.invalid",
                  "style": { "text": "#00ff00" /* the only colour */ },
                  /* trailing */
                },
                // and nothing after this one
              ],
            }"##,
    );
    assert!(dense.notes.is_empty(), "{:?}", dense.notes);
    let family = dense.family.expect("a family");
    assert_eq!(family.themes[0].shell.text, 0x00ff00);
    assert_eq!(
        family.themes[0].name, "https://example.invalid",
        "a slash pair inside a string is text, not a comment"
    );
}
