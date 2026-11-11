//! Search and replace over the buffer, with the shell's regex discipline.
//!
//! Patterns compile case-insensitive with the same 1 MiB compiled-size cap
//! as the read-only text view, and an invalid pattern is a labelled state a
//! status line can print -- never a panic and never a silent no-match. The
//! match set is byte ranges over a materialization of the rope, refreshed
//! only when the buffer version moves, and replacement expands `$1`-style
//! capture references through the regex crate's own expansion rules. A
//! replacement reports the [`Splice`]s it made, because a replacement is an
//! ordinary edit: the view feeds them to the incremental reparse rather than
//! throwing the tree away.

use crate::buffer::{Buffer, EditGroup, Selection, SelectionIntent, Splice};
use std::ops::Range;

const MAX_PATTERN_BYTES: usize = 1 << 20;
const MAX_ERROR_CHARS: usize = 120;

#[derive(Debug)]
pub struct SearchState {
    query: String,
    regex: bool,
    pattern: Result<regex::Regex, String>,
    matches: Vec<Range<usize>>,
    current: usize,
    matched_version: Option<u64>,
}

impl SearchState {
    pub fn new(query: &str, regex: bool) -> SearchState {
        let pattern = compile(query, regex);
        SearchState {
            query: query.to_string(),
            regex,
            pattern,
            matches: Vec::new(),
            current: 0,
            matched_version: None,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_regex(&self) -> bool {
        self.regex
    }

    pub fn error(&self) -> Option<&str> {
        self.pattern.as_ref().err().map(String::as_str)
    }

    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    pub fn current(&self) -> Option<Range<usize>> {
        self.matches.get(self.current).cloned()
    }

    pub fn current_index(&self) -> usize {
        self.current
    }

    pub fn refresh(&mut self, buffer: &Buffer) {
        if self.matched_version == Some(buffer.version()) {
            return;
        }
        self.matches.clear();
        if let Ok(pattern) = &self.pattern
            && !self.query.is_empty()
        {
            let text = buffer.text();
            for found in pattern.find_iter(&text) {
                if found.range().is_empty() {
                    break;
                }
                self.matches.push(found.range());
            }
        }
        self.current = self.current.min(self.matches.len().saturating_sub(1));
        self.matched_version = Some(buffer.version());
    }

    pub fn jump_from(&mut self, offset: usize) {
        self.current = self
            .matches
            .iter()
            .position(|found| found.start >= offset)
            .unwrap_or(0);
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }

    pub fn select_current(&self, buffer: &mut Buffer) {
        if let Some(found) = self.current() {
            buffer.set_selections(vec![Selection::range(found.start, found.end)], 0);
        }
    }

    pub fn replace_current(&mut self, buffer: &mut Buffer, replacement: &str) -> Replacement {
        self.refresh(buffer);
        let (Ok(pattern), Some(found)) = (&self.pattern, self.current()) else {
            return Replacement::none();
        };
        let text = buffer.text();
        let mut replaced = String::new();
        let Some(captures) = pattern.captures_at(&text, found.start) else {
            return Replacement::none();
        };
        if captures.get(0).map(|whole| whole.range()) != Some(found.clone()) {
            return Replacement::none();
        }
        expand(pattern, &captures, replacement, self.regex, &mut replaced);
        // One replacement is one cursor's edit, so the caret lands after the
        // text it just wrote and `next` moves on from there.
        let splices = buffer.edit(
            vec![(found, replaced)],
            EditGroup::Other,
            SelectionIntent::Collapse,
        );
        self.refresh(buffer);
        Replacement { count: 1, splices }
    }

    pub fn replace_all(&mut self, buffer: &mut Buffer, replacement: &str) -> Replacement {
        self.refresh(buffer);
        let Ok(pattern) = &self.pattern else {
            return Replacement::none();
        };
        if self.matches.is_empty() {
            return Replacement::none();
        }
        let text = buffer.text();
        let mut edits = Vec::with_capacity(self.matches.len());
        for captures in pattern.captures_iter(&text) {
            let whole = captures
                .get(0)
                .expect("capture zero is the whole match")
                .range();
            if whole.is_empty() {
                break;
            }
            let mut replaced = String::new();
            expand(pattern, &captures, replacement, self.regex, &mut replaced);
            edits.push((whole, replaced));
        }
        let count = edits.len();
        // Replacing everywhere is not a cursor move: whatever the user had
        // selected stays selected, shifted by the edits.
        let splices = buffer.edit(edits, EditGroup::Other, SelectionIntent::Preserve);
        self.refresh(buffer);
        Replacement { count, splices }
    }
}

// What a replacement did: how many matches it rewrote and the splices that
// rewrote them, in application order.
#[derive(Debug, Default)]
pub struct Replacement {
    pub count: usize,
    pub splices: Vec<Splice>,
}

impl Replacement {
    fn none() -> Replacement {
        Replacement::default()
    }

    pub fn happened(&self) -> bool {
        self.count > 0
    }
}

fn compile(query: &str, regex: bool) -> Result<regex::Regex, String> {
    let source = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    regex::RegexBuilder::new(&source)
        .case_insensitive(true)
        .size_limit(MAX_PATTERN_BYTES)
        .build()
        .map_err(|error| one_line(&error.to_string()))
}

fn expand(
    pattern: &regex::Regex,
    captures: &regex::Captures<'_>,
    replacement: &str,
    regex: bool,
    into: &mut String,
) {
    let _ = pattern;
    if regex {
        captures.expand(replacement, into);
    } else {
        into.push_str(replacement);
    }
}

// The error carries the user's own pattern text back to the status line, so
// the shortening counts characters: byte 120 of an arbitrary regex can sit
// inside a multi-byte character, and `String::truncate` panics there.
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shortened: String = flat.chars().take(MAX_ERROR_CHARS).collect();
    if shortened.len() < flat.len() {
        shortened.push('…');
    }
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_query_finds_case_insensitive_matches() {
        let buffer = Buffer::new("Name: a\nname: b\nNAME: c\n");
        let mut search = SearchState::new("name", false);
        search.refresh(&buffer);
        assert_eq!(search.matches().len(), 3);
    }

    #[test]
    fn an_invalid_pattern_is_a_labelled_state_never_a_panic() {
        let buffer = Buffer::new("anything");
        let mut search = SearchState::new("[unclosed", true);
        search.refresh(&buffer);
        assert!(search.error().is_some());
        assert!(search.matches().is_empty());
    }

    #[test]
    fn a_literal_query_with_regex_metacharacters_matches_itself() {
        let buffer = Buffer::new("port: [8080]\n");
        let mut search = SearchState::new("[8080]", false);
        search.refresh(&buffer);
        assert_eq!(search.matches().len(), 1);
    }

    #[test]
    fn next_and_prev_wrap_around_the_match_set() {
        let buffer = Buffer::new("a b a b a");
        let mut search = SearchState::new("a", false);
        search.refresh(&buffer);
        assert_eq!(search.matches().len(), 3);
        search.next();
        search.next();
        assert_eq!(search.current_index(), 2);
        search.next();
        assert_eq!(search.current_index(), 0, "next wraps forward");
        search.prev();
        assert_eq!(search.current_index(), 2, "prev wraps backward");
    }

    #[test]
    fn replace_all_expands_capture_references_in_regex_mode() {
        let mut buffer = Buffer::new("image: nginx:1.27\nimage: redis:7.2\n");
        let mut search = SearchState::new(r"image: (\w+):([\d.]+)", true);
        let replaced = search.replace_all(&mut buffer, "image: $1:latest");
        assert_eq!(replaced.count, 2);
        assert_eq!(buffer.text(), "image: nginx:latest\nimage: redis:latest\n");
    }

    #[test]
    fn replace_in_literal_mode_never_expands_dollar_signs() {
        let mut buffer = Buffer::new("value: cost\n");
        let mut search = SearchState::new("cost", false);
        search.refresh(&buffer);
        assert!(search.replace_current(&mut buffer, "$100").happened());
        assert_eq!(buffer.text(), "value: $100\n");
    }

    #[test]
    fn replace_current_moves_on_and_the_match_set_follows_the_edit() {
        let mut buffer = Buffer::new("a a a");
        let mut search = SearchState::new("a", false);
        search.refresh(&buffer);
        assert!(search.replace_current(&mut buffer, "bb").happened());
        assert_eq!(buffer.text(), "bb a a");
        assert_eq!(
            search.matches().len(),
            2,
            "matches recompute after the edit"
        );
    }

    #[test]
    fn a_replacement_reports_the_splices_that_made_it() {
        let mut buffer = Buffer::new("a\na\n");
        let mut search = SearchState::new("a", false);
        let replaced = search.replace_all(&mut buffer, "bb");
        assert_eq!(replaced.count, 2);
        assert_eq!(
            replaced.splices.len(),
            2,
            "an incremental reparse needs one splice per edit"
        );
        assert!(
            replaced.splices[0].start > replaced.splices[1].start,
            "splices arrive in application order, back to front: {:?}",
            replaced.splices
        );
    }

    #[test]
    fn a_pattern_error_shortens_on_a_character_boundary() {
        // The error text carries the user's own pattern, and a multi-byte
        // character straddling the cut is a panic if the cut counts bytes.
        let long = "\u{1F600}".repeat(200);
        let buffer = Buffer::new("anything");
        let mut search = SearchState::new(&format!("[{long}"), true);
        search.refresh(&buffer);
        let error = search.error().expect("an unclosed class is an error");
        assert!(error.chars().count() <= MAX_ERROR_CHARS + 1);
        assert!(error.ends_with('…'), "the shortening is visible: {error}");
    }

    #[test]
    fn an_empty_match_never_spins_the_scan() {
        let buffer = Buffer::new("abc");
        let mut search = SearchState::new("x*", true);
        search.refresh(&buffer);
        assert!(
            search.matches().is_empty(),
            "empty-width matches are refused"
        );
    }

    #[test]
    fn jump_from_picks_the_match_at_or_after_the_cursor() {
        let buffer = Buffer::new("a b a b a");
        let mut search = SearchState::new("a", false);
        search.refresh(&buffer);
        search.jump_from(3);
        assert_eq!(search.current(), Some(4..5));
        search.jump_from(9);
        assert_eq!(
            search.current(),
            Some(0..1),
            "past the last match wraps home"
        );
    }
}
