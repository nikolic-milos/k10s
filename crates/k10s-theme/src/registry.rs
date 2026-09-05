//! Every theme this session can name.
//!
//! The registry is what makes a user theme indistinguishable from a shipped
//! one: both arrive as a [`ThemeFamily`], both answer to their name, and the
//! settings schema completes both from the same list. A family added later
//! shadows an earlier one of the same theme name, so a user who writes their
//! own `k10s-dark` gets theirs -- Zed behaves the same way, and the
//! alternative is a theme file that silently does nothing.

use std::sync::Arc;

use gpui::SharedString;

use crate::{Appearance, Theme, ThemeFamily, builtin_family};

// The two names the very first theme setting accepted. Dropping them would
// turn a working settings file into a note and a surprise change of colour,
// so they stay resolvable even though nothing advertises them as themes.
const ALIASES: [(&str, &str); 2] = [("starmap-dark", "one-dark"), ("one dark", "one-dark")];

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeRegistry {
    families: Vec<ThemeFamily>,
}

impl ThemeRegistry {
    pub fn builtin() -> ThemeRegistry {
        ThemeRegistry {
            families: vec![builtin_family()],
        }
    }

    pub fn add_family(&mut self, family: ThemeFamily) {
        self.families.push(family);
    }

    pub fn families(&self) -> &[ThemeFamily] {
        &self.families
    }

    pub fn themes(&self) -> impl Iterator<Item = &Arc<Theme>> {
        self.families.iter().flat_map(|family| family.themes.iter())
    }

    /// Resolve a settings value. Case and surrounding space are the user's
    /// business, not ours.
    pub fn get(&self, name: &str) -> Option<&Arc<Theme>> {
        let name = name.trim();
        if let Some(found) = self.find_exact(name) {
            return Some(found);
        }
        ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
            .and_then(|(_, target)| self.find_exact(target))
    }

    fn find_exact(&self, name: &str) -> Option<&Arc<Theme>> {
        self.families.iter().rev().find_map(|family| {
            family
                .themes
                .iter()
                .find(|theme| theme.name.eq_ignore_ascii_case(name))
        })
    }

    /// The theme a `"mode": "system"` setting lands on: the shipped brand
    /// theme of that appearance, resolved by name so that a user file which
    /// reuses the name is honoured here too rather than only when the name is
    /// typed out.
    pub fn default_for(&self, appearance: Appearance) -> &Arc<Theme> {
        let shipped = self.families[0]
            .themes
            .iter()
            .find(|theme| theme.appearance == appearance)
            .unwrap_or(&self.families[0].themes[0]);
        self.find_exact(&shipped.name).unwrap_or(shipped)
    }

    /// Every name a settings file may use, in registration order, for the
    /// schema's completion list. Aliases are appended because the loader
    /// accepts them and a schema that flags a value the loader accepts is a
    /// worse lie than one that offers a deprecated name.
    ///
    /// Deduplicated the way [`get`](Self::get) resolves -- ignoring case --
    /// because a user file that respells a shipped name shadows it rather than
    /// adding a theme, and offering both spellings would promise a choice that
    /// does not exist.
    pub fn names(&self) -> Vec<SharedString> {
        let mut names: Vec<SharedString> = Vec::new();
        let push = |name: SharedString, names: &mut Vec<SharedString>| {
            if !names
                .iter()
                .any(|seen| seen.eq_ignore_ascii_case(name.as_ref()))
            {
                names.push(name);
            }
        };
        for theme in self.themes() {
            push(theme.name.clone(), &mut names);
        }
        for (alias, _) in ALIASES {
            push(alias.into(), &mut names);
        }
        names
    }
}

impl Default for ThemeRegistry {
    fn default() -> ThemeRegistry {
        ThemeRegistry::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_family;

    #[test]
    fn the_registry_finds_themes_by_name_case_insensitively() {
        let registry = ThemeRegistry::builtin();
        assert!(registry.get("one-dark").is_some());
        assert!(registry.get("  One Dark ").is_some());
        assert!(registry.get("K10S-Dark").is_some());
        assert!(
            registry.get("starmap-dark").is_some(),
            "the oldest setting remains an alias"
        );
        assert!(registry.get("does-not-exist").is_none());
    }

    #[test]
    fn the_default_for_an_appearance_is_the_brand_theme_of_that_appearance() {
        let registry = ThemeRegistry::builtin();
        assert_eq!(registry.default_for(Appearance::Dark).name, "k10s-dark");
        assert_eq!(registry.default_for(Appearance::Light).name, "k10s-light");
    }

    #[test]
    fn a_user_family_joins_the_list_and_shadows_a_name_it_reuses() {
        let mut registry = ThemeRegistry::builtin();
        let loaded = parse_family(
            r##"{ "name": "Mine", "themes": [
                 { "name": "Mine", "style": { "text": "#ffffff" } },
                 { "name": "k10s-dark", "style": { "text": "#ff0000" } }
               ] }"##,
        );
        registry.add_family(loaded.family.expect("a family"));

        assert_eq!(registry.get("mine").expect("found").shell.text, 0xffffff);
        assert_eq!(
            registry.get("k10s-dark").expect("found").shell.text,
            0xff0000,
            "the file a user wrote wins over the one we shipped"
        );
        assert!(
            registry.names().iter().any(|name| name == "Mine"),
            "a user theme completes: {:?}",
            registry.names()
        );
        assert_eq!(
            registry.default_for(Appearance::Dark).shell.text,
            0xff0000,
            "shadowing a built-in name shadows it everywhere, including the default"
        );
    }

    #[test]
    fn a_respelt_name_completes_once_because_it_resolves_once() {
        let mut registry = ThemeRegistry::builtin();
        let before = registry.names().len();
        let loaded = parse_family(
            r##"{ "name": "Mine", "themes": [
                 { "name": "K10S-Dark", "style": { "text": "#ff0000" } }
               ] }"##,
        );
        registry.add_family(loaded.family.expect("a family"));

        let names = registry.names();
        assert_eq!(
            names.len(),
            before,
            "a respelling shadows a theme rather than adding one: {names:?}"
        );
        for name in &names {
            assert!(registry.get(name).is_some(), "{name} does not resolve");
        }
        assert_eq!(
            registry.get("k10s-dark").expect("found").shell.text,
            0xff0000,
            "and the file a user wrote still wins"
        );
    }

    #[test]
    fn every_advertised_name_resolves() {
        let registry = ThemeRegistry::builtin();
        let names = registry.names();
        assert_eq!(names.len(), 5, "{names:?}");
        for name in &names {
            assert!(registry.get(name).is_some(), "{name} does not resolve");
        }
    }
}
