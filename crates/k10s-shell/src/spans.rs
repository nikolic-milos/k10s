//! Pure run composition: every decoration a visible line carries, resolved to
//! disjoint spans with one priority order, testable without a window.
//!
//! A row can be under a syntax token, inside a selection, under the caret, on a
//! search match and under a diagnostic all at once, and gpui wants a flat list of
//! non-overlapping styled runs. Resolving that here rather than in the element
//! is what makes the priority order a unit test instead of something judged by
//! looking at the screen -- and what lets the editor bench measure a viewport's
//! worth of composition with no window at all.

use std::ops::Range;

use gpui::{HighlightStyle, UnderlineStyle, px, rgb};

use k10s_edit::complete::DiagnosticSeverity;
use k10s_edit::{Diagnostic, Rope, Selection, TokenKind};
use k10s_theme::Theme;

// ---------------------------------------------------------------------------
// Pure run composition: every decoration a visible line carries, resolved to
// disjoint spans with one priority order, testable without a window.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpanFlags {
    pub token: Option<TokenKind>,
    pub selected: bool,
    pub caret: bool,
    pub matched: bool,
    pub current_match: bool,
    pub diagnostic: Option<DiagnosticSeverity>,
}

impl SpanFlags {
    pub(crate) fn any(&self) -> bool {
        self.token.is_some()
            || self.selected
            || self.caret
            || self.matched
            || self.current_match
            || self.diagnostic.is_some()
    }
}

#[derive(Debug, Default)]
pub struct LineLayers {
    pub tokens: Vec<(Range<usize>, TokenKind)>,
    pub selections: Vec<Range<usize>>,
    pub carets: Vec<Range<usize>>,
    pub matches: Vec<Range<usize>>,
    pub current_match: Option<Range<usize>>,
    pub diagnostics: Vec<(Range<usize>, DiagnosticSeverity)>,
}

pub fn compose_line(len: usize, layers: &LineLayers) -> Vec<(Range<usize>, SpanFlags)> {
    let mut boundaries = vec![0, len];
    let mut collect = |range: &Range<usize>| {
        boundaries.push(range.start.min(len));
        boundaries.push(range.end.min(len));
    };
    for (range, _) in &layers.tokens {
        collect(range);
    }
    for range in &layers.selections {
        collect(range);
    }
    for range in &layers.carets {
        collect(range);
    }
    for range in &layers.matches {
        collect(range);
    }
    if let Some(range) = &layers.current_match {
        collect(range);
    }
    for (range, _) in &layers.diagnostics {
        collect(range);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut spans = Vec::new();
    for pair in boundaries.windows(2) {
        let segment = pair[0]..pair[1];
        if segment.is_empty() {
            continue;
        }
        let covers =
            |range: &Range<usize>| range.start <= segment.start && range.end >= segment.end;
        let flags = SpanFlags {
            token: layers
                .tokens
                .iter()
                .rev()
                .find(|(range, _)| covers(range))
                .map(|(_, token)| *token),
            selected: layers.selections.iter().any(covers),
            caret: layers.carets.iter().any(covers),
            matched: layers.matches.iter().any(covers),
            current_match: layers.current_match.as_ref().is_some_and(covers),
            diagnostic: layers
                .diagnostics
                .iter()
                .find(|(range, _)| covers(range))
                .map(|(_, severity)| *severity),
        };
        if flags.any() {
            spans.push((segment, flags));
        }
    }
    spans
}

pub(crate) fn token_color(theme: &Theme, token: TokenKind) -> u32 {
    let syntax = &theme.syntax;
    match token {
        TokenKind::Property => syntax.property,
        TokenKind::Str => syntax.string,
        TokenKind::Number => syntax.number,
        TokenKind::Boolean => syntax.boolean,
        TokenKind::Constant => syntax.constant,
        TokenKind::Comment => syntax.comment,
        TokenKind::Anchor => syntax.label,
        TokenKind::Tag => syntax.type_name,
        TokenKind::Directive => syntax.attribute,
        TokenKind::Punctuation => syntax.punctuation,
        TokenKind::PunctuationSpecial => syntax.punctuation_special,
    }
}

pub(crate) fn flag_style(theme: &Theme, flags: SpanFlags) -> HighlightStyle {
    let mut style = HighlightStyle::default();
    if let Some(token) = flags.token {
        style.color = Some(rgb(token_color(theme, token)).into());
    }
    if flags.matched {
        let (color, alpha) = theme.shell.search_match_background;
        style.background_color = Some(rgb(color).alpha(alpha * 0.6).into());
    }
    if flags.selected {
        let (color, alpha) = theme.syntax.selection_background;
        style.background_color = Some(rgb(color).alpha(alpha).into());
    }
    if flags.current_match {
        let (color, alpha) = theme.shell.search_match_background;
        style.background_color = Some(rgb(color).alpha(alpha).into());
    }
    if let Some(severity) = flags.diagnostic {
        let color = match severity {
            DiagnosticSeverity::Error => theme.shell.error,
            DiagnosticSeverity::Warning => theme.shell.warning,
        };
        style.underline = Some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(rgb(color).into()),
            wavy: true,
        });
    }
    if flags.caret {
        style.background_color = Some(rgb(theme.shell.cursor).into());
        style.color = Some(rgb(theme.shell.editor_background).into());
    }
    style
}

// Every decoration the visible rows carry, resolved in one pass. The shipped
// render path is what a frame budget applies to, so it asks the tree once for
// the whole viewport instead of once per row, and walks the sorted disjoint
// decorations forward with the rows instead of rescanning every one of them per
// row -- at a megabyte the search set alone is thousands of ranges. Diagnostics
// are capped at a couple of hundred by the validator, so those are scanned
// whole and the bound is the reason.
pub fn viewport_layers(
    rope: &Rope,
    tokens: &[(Range<usize>, TokenKind)],
    selections: &[Selection],
    matches: &[Range<usize>],
    current_match: Option<&Range<usize>>,
    diagnostics: &[Diagnostic],
    rows: Range<usize>,
) -> Vec<LineLayers> {
    let mut all = Vec::with_capacity(rows.end.saturating_sub(rows.start));
    let mut token_at = 0usize;
    let mut match_at = 0usize;
    let mut selection_at = 0usize;
    for row in rows {
        let start = rope.line_start(row);
        let end = start + rope.line_len(row);
        // A caret sitting past the last glyph paints on the padding column, so
        // the clip reaches one byte beyond the line.
        let clip = |range: &Range<usize>| -> Option<Range<usize>> {
            if range.end <= start || range.start > end {
                return None;
            }
            Some(range.start.max(start) - start..range.end.min(end + 1) - start)
        };
        let mut layers = LineLayers::default();

        while token_at < tokens.len() && tokens[token_at].0.end <= start {
            token_at += 1;
        }
        for (range, token) in &tokens[token_at..] {
            if range.start > end {
                break;
            }
            if let Some(local) = clip(range) {
                layers.tokens.push((local, *token));
            }
        }

        while selection_at < selections.len() && selections[selection_at].end() < start {
            selection_at += 1;
        }
        for selection in &selections[selection_at..] {
            if selection.start() > end {
                break;
            }
            if !selection.is_caret()
                && let Some(local) = clip(&(selection.start()..selection.end()))
            {
                layers.selections.push(local);
            }
            let head = selection.head;
            if head >= start && head <= end {
                let caret_end = if head < end {
                    // A CRLF is one cluster but the line keeps its CR, so the
                    // step can land on the next row; the caret stays on this one.
                    (rope.next_grapheme_offset(head) - start).min(end - start + 1)
                } else {
                    head - start + 1
                };
                layers.carets.push(head - start..caret_end);
            }
        }

        while match_at < matches.len() && matches[match_at].end <= start {
            match_at += 1;
        }
        for range in &matches[match_at..] {
            if range.start > end {
                break;
            }
            if let Some(local) = clip(range) {
                if current_match == Some(range) {
                    layers.current_match = Some(local);
                } else {
                    layers.matches.push(local);
                }
            }
        }

        for diagnostic in diagnostics {
            if let Some(local) = clip(&diagnostic.range) {
                layers.diagnostics.push((local, diagnostic.severity));
            }
        }
        all.push(layers);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers() -> LineLayers {
        LineLayers::default()
    }

    #[test]
    fn compose_splits_a_token_around_a_caret() {
        let mut line = layers();
        line.tokens.push((0..10, TokenKind::Property));
        line.carets.push(4..5);
        let spans = compose_line(11, &line);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].0, 0..4);
        assert!(spans[0].1.token.is_some() && !spans[0].1.caret);
        assert_eq!(spans[1].0, 4..5);
        assert!(spans[1].1.caret && spans[1].1.token.is_some());
        assert_eq!(spans[2].0, 5..10);
        assert!(!spans[2].1.caret);
    }

    #[test]
    fn compose_layers_selection_over_tokens_without_overlap() {
        let mut line = layers();
        line.tokens.push((0..4, TokenKind::Property));
        line.tokens.push((6..11, TokenKind::Str));
        line.selections.push(2..8);
        let spans = compose_line(12, &line);
        for pair in spans.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "{spans:?}");
        }
        let selected: Vec<_> = spans.iter().filter(|(_, flags)| flags.selected).collect();
        assert_eq!(selected.first().expect("selection renders").0.start, 2);
        assert_eq!(selected.last().expect("selection renders").0.end, 8);
    }

    #[test]
    fn compose_keeps_diagnostics_and_matches_together() {
        let mut line = layers();
        line.matches.push(0..4);
        line.current_match = Some(5..9);
        line.diagnostics.push((2..9, DiagnosticSeverity::Warning));
        let spans = compose_line(10, &line);
        let both = spans
            .iter()
            .find(|(range, _)| range.start == 2)
            .expect("overlap segment exists");
        assert!(both.1.matched && both.1.diagnostic.is_some());
        let current = spans
            .iter()
            .find(|(range, _)| range.start == 5)
            .expect("current match segment exists");
        assert!(current.1.current_match);
    }

    #[test]
    fn an_unflagged_segment_is_omitted_for_the_base_style() {
        let mut line = layers();
        line.tokens.push((5..8, TokenKind::Number));
        let spans = compose_line(20, &line);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 5..8);
    }

    #[test]
    fn the_viewport_only_looks_at_the_rows_it_paints() {
        let rope = Rope::from(
            "alpha
beta
gamma
delta
",
        );
        let tokens = [(0..5, TokenKind::Property), (11..16, TokenKind::Str)];
        let matches = [0..2, 6..8, 11..13, 17..19];
        let selections = [Selection::caret(12)];
        let diagnostics = [Diagnostic {
            range: 17..22,
            severity: DiagnosticSeverity::Error,
            message: String::new(),
        }];
        let rows = viewport_layers(
            &rope,
            &tokens,
            &selections,
            &matches,
            Some(&(11..13)),
            &diagnostics,
            1..3,
        );
        assert_eq!(rows.len(), 2, "one layer set per painted row");
        assert!(
            rows[0].tokens.is_empty() && rows[0].current_match.is_none(),
            "row one carries only its own match: {:?}",
            rows[0]
        );
        assert_eq!(
            rows[0].matches.first(),
            Some(&(0..2)),
            "beta's match, in line coordinates"
        );
        assert_eq!(rows[0].matches.len(), 1, "and only that one");
        assert_eq!(rows[1].tokens, [(0..5, TokenKind::Str)]);
        assert_eq!(
            rows[1].current_match,
            Some(0..2),
            "the current match is the one the search is on"
        );
        assert_eq!(
            rows[1].carets.first(),
            Some(&(1..2)),
            "and the caret is local too"
        );
        assert!(
            rows[0].diagnostics.is_empty() && rows[1].diagnostics.is_empty(),
            "the diagnostic is on a row nobody painted"
        );
    }

    #[test]
    fn a_span_reaching_across_rows_is_clipped_into_each_of_them() {
        let rope = Rope::from(
            "one
two
three
",
        );
        let tokens = [(0..13, TokenKind::Comment)];
        let rows = viewport_layers(&rope, &tokens, &[], &[], None, &[], 0..3);
        assert_eq!(rows[0].tokens, [(0..4, TokenKind::Comment)]);
        assert_eq!(rows[1].tokens, [(0..4, TokenKind::Comment)]);
        assert_eq!(rows[2].tokens, [(0..5, TokenKind::Comment)]);
    }
}
