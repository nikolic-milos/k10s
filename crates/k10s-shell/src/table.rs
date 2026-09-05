//! Pure table state: columns, rows, selection, filter, scroll.
//!
//! The browser and the node view are the same machine over different pages,
//! so the machine lives here with no gpui in sight. Layout is monospace
//! column packing -- widths from content, capped, two-space gutters -- and
//! selection is keyed by row identity across refetches, not by index, for
//! the same reason the map keys selection by uid: rows move. Continuation
//! pages append behind the rows already held, up to a hard cap of
//! [`MAX_ROWS`]; past it the machine refuses more and says so.

use crate::provider::{TablePage, TableRow};

const MIN_COL: usize = 2;
const MAX_COL: usize = 48;
const GUTTER: &str = "  ";

// Ten server pages of 500. A list bigger than this wants a filter, not a
// longer scrollbar; the cap keeps the shell's memory bounded whatever the
// cluster holds.
pub const MAX_ROWS: usize = 5_000;

#[derive(Debug, Default)]
pub struct TableState {
    page: TablePage,
    visible: Vec<usize>,
    widths: Vec<usize>,
    pub filter: String,
    selected: usize,
    top: usize,
    viewport: usize,
    capped: bool,
}

impl TableState {
    pub fn new() -> TableState {
        TableState {
            viewport: 1,
            ..TableState::default()
        }
    }

    pub fn set_page(&mut self, page: TablePage) {
        let keep = self.selected_row().map(|row| row.uid.clone());
        self.page = page;
        self.capped = false;
        self.enforce_cap();
        self.recompute(keep.as_deref());
    }

    // A continuation page: rows join behind the ones already held, the new
    // page's token replaces the old one, and the selection stays where the
    // user put it. Columns keep the first page's shape -- the server renders
    // the same table either way.
    pub fn append_page(&mut self, page: TablePage) {
        let keep = self.selected_row().map(|row| row.uid.clone());
        self.page.rows.extend(page.rows);
        self.page.truncated = page.truncated;
        self.page.continue_token = page.continue_token;
        self.enforce_cap();
        self.recompute(keep.as_deref());
    }

    fn enforce_cap(&mut self) {
        if self.page.rows.len() > MAX_ROWS {
            self.page.rows.truncate(MAX_ROWS);
            self.page.continue_token = None;
            self.capped = true;
        }
    }

    pub fn truncated(&self) -> bool {
        self.page.truncated
    }

    pub fn capped(&self) -> bool {
        self.capped
    }

    pub fn continue_token(&self) -> Option<&str> {
        self.page.continue_token.as_deref()
    }

    pub fn total_rows(&self) -> usize {
        self.page.rows.len()
    }

    pub fn visible_rows(&self) -> usize {
        self.visible.len()
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.ensure_selected_visible();
    }

    pub fn push_filter(&mut self, text: &str) {
        self.filter.push_str(text);
        self.recompute(self.selected_uid().as_deref());
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.recompute(self.selected_uid().as_deref());
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.recompute(self.selected_uid().as_deref());
    }

    fn selected_uid(&self) -> Option<String> {
        self.selected_row().map(|row| row.uid.clone())
    }

    fn recompute(&mut self, keep_uid: Option<&str>) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .page
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty() || row.cells.iter().any(|cell| contains_folded(cell, &needle))
            })
            .map(|(index, _)| index)
            .collect();

        self.widths = self
            .page
            .columns
            .iter()
            .map(|column| column.name.chars().count())
            .collect();
        for index in &self.visible {
            for (at, cell) in self.page.rows[*index].cells.iter().enumerate() {
                if at < self.widths.len() {
                    self.widths[at] = self.widths[at].max(cell.chars().count());
                }
            }
        }
        for width in &mut self.widths {
            *width = (*width).clamp(MIN_COL, MAX_COL);
        }

        self.selected = keep_uid
            .and_then(|uid| {
                self.visible
                    .iter()
                    .position(|index| self.page.rows[*index].uid == uid)
            })
            .unwrap_or_else(|| self.selected.min(self.visible.len().saturating_sub(1)));
        self.ensure_selected_visible();
    }

    pub fn selected_row(&self) -> Option<&TableRow> {
        self.visible
            .get(self.selected)
            .map(|index| &self.page.rows[*index])
    }

    pub fn selected_cell(&self, column: &str) -> Option<&str> {
        let index = self
            .page
            .columns
            .iter()
            .position(|candidate| candidate.name == column)?;
        self.selected_row()?.cells.get(index).map(String::as_str)
    }

    pub fn move_selection(&mut self, delta: i64) {
        if self.visible.is_empty() {
            return;
        }
        let last = (self.visible.len() - 1) as i64;
        self.selected = (self.selected as i64 + delta).clamp(0, last) as usize;
        self.ensure_selected_visible();
    }

    pub fn page_by(&mut self, direction: i64) {
        self.move_selection(direction * (self.viewport.saturating_sub(1)).max(1) as i64);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.ensure_selected_visible();
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
        self.ensure_selected_visible();
    }

    pub fn select_visible_offset(&mut self, offset: usize) {
        if self.top + offset < self.visible.len() {
            self.selected = self.top + offset;
        }
    }

    fn ensure_selected_visible(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        }
        let last_visible = self.top + self.viewport.saturating_sub(1);
        if self.selected > last_visible {
            self.top = self.selected - self.viewport.saturating_sub(1);
        }
        self.top = self.top.min(
            self.visible
                .len()
                .saturating_sub(self.viewport.min(self.visible.len())),
        );
    }

    pub fn header_line(&self) -> String {
        let names: Vec<String> = self
            .page
            .columns
            .iter()
            .map(|column| column.name.to_uppercase())
            .collect();
        format_cells(&names, &self.widths)
    }

    // The rows on screen: (is_selected, formatted line).
    pub fn visible_lines(&self) -> Vec<(bool, String)> {
        self.visible
            .iter()
            .enumerate()
            .skip(self.top)
            .take(self.viewport)
            .map(|(at, index)| {
                (
                    at == self.selected,
                    format_cells(&self.page.rows[*index].cells, &self.widths),
                )
            })
            .collect()
    }
}

// Whether a cell holds an already-lowercased needle, ignoring case. A filter
// keystroke asks this of every cell of every row -- five thousand of them at
// the cap -- so the ASCII answer, which is nearly every cell a cluster has,
// is given without lowercasing a copy of the cell to throw away. A needle that
// is not ASCII cannot be inside a cell that is.
fn contains_folded(cell: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if cell.is_ascii() {
        return needle.is_ascii()
            && cell
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()));
    }
    cell.to_lowercase().contains(needle)
}

fn format_cells(cells: &[String], widths: &[usize]) -> String {
    let mut out = String::new();
    for (at, width) in widths.iter().enumerate() {
        if at > 0 {
            out.push_str(GUTTER);
        }
        let cell = cells.get(at).map(String::as_str).unwrap_or("");
        let count = cell.chars().count();
        if count > *width {
            out.extend(cell.chars().take(width.saturating_sub(1)));
            out.push('\u{2026}');
        } else {
            out.push_str(cell);
            for _ in count..*width {
                out.push(' ');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::TableColumn;

    fn page(rows: &[(&str, &[&str])]) -> TablePage {
        let columns = ["Name", "Ready"]
            .iter()
            .map(|name| TableColumn {
                name: name.to_string(),
                wide: false,
            })
            .collect();
        TablePage {
            columns,
            rows: rows
                .iter()
                .map(|(uid, cells)| TableRow {
                    cells: cells.iter().map(|c| c.to_string()).collect(),
                    name: cells[0].to_string(),
                    namespace: None,
                    uid: uid.to_string(),
                })
                .collect(),
            truncated: false,
            continue_token: None,
        }
    }

    fn numbered_page(range: std::ops::Range<usize>, token: Option<&str>) -> TablePage {
        TablePage {
            columns: vec![TableColumn {
                name: "Name".to_string(),
                wide: false,
            }],
            rows: range
                .map(|i| TableRow {
                    cells: vec![format!("row-{i}")],
                    name: format!("row-{i}"),
                    namespace: None,
                    uid: format!("u{i}"),
                })
                .collect(),
            truncated: token.is_some(),
            continue_token: token.map(str::to_string),
        }
    }

    #[test]
    fn selection_is_keyed_by_uid_across_refetches() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(page(&[
            ("u1", &["api", "1/1"]),
            ("u2", &["web", "0/1"]),
            ("u3", &["job", "1/1"]),
        ]));
        table.move_selection(1);
        assert_eq!(table.selected_row().unwrap().uid, "u2");

        table.set_page(page(&[
            ("u3", &["job", "1/1"]),
            ("u2", &["web", "1/1"]),
            ("u1", &["api", "1/1"]),
        ]));
        assert_eq!(
            table.selected_row().unwrap().uid,
            "u2",
            "the row moved, the selection did not"
        );
    }

    #[test]
    fn a_selected_cell_is_resolved_by_column_name_after_rows_move() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(page(&[
            ("u1", &["api", "Ready"]),
            ("u2", &["worker", "NotReady"]),
        ]));
        table.move_selection(1);

        assert_eq!(table.selected_cell("Ready"), Some("NotReady"));
        assert_eq!(table.selected_cell("missing"), None);
    }

    #[test]
    fn the_filter_matches_any_cell_case_insensitively() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(page(&[
            ("u1", &["api", "1/1"]),
            ("u2", &["WEB", "0/1"]),
            ("u3", &["wedge", "1/1"]),
        ]));
        table.push_filter("we");
        assert_eq!(table.visible_rows(), 2, "WEB and wedge");
        table.push_filter("b");
        assert_eq!(table.visible_rows(), 1);
        assert_eq!(table.selected_row().unwrap().uid, "u2");
        table.clear_filter();
        assert_eq!(table.visible_rows(), 3);
        assert_eq!(
            table.selected_row().unwrap().uid,
            "u2",
            "clearing the filter keeps the row"
        );
    }

    #[test]
    fn the_filter_folds_case_on_both_sides_of_the_ascii_split() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(page(&[
            ("u1", &["BackOff", "0/1"]),
            ("u2", &["Ärger-api", "1/1"]),
            ("u3", &["quiet", "1/1"]),
        ]));

        table.push_filter("backoff");
        assert_eq!(table.visible_rows(), 1, "an ASCII cell folds its own case");
        assert_eq!(table.selected_row().unwrap().uid, "u1");

        table.clear_filter();
        table.push_filter("ärger");
        assert_eq!(
            table.visible_rows(),
            1,
            "and a cell that is not ASCII still folds"
        );
        assert_eq!(table.selected_row().unwrap().uid, "u2");

        table.clear_filter();
        table.push_filter("ä");
        assert_eq!(
            table.visible_rows(),
            1,
            "a needle that is not ASCII matches only the row that has it"
        );
        assert_eq!(table.selected_row().unwrap().uid, "u2");
    }

    #[test]
    fn column_widths_come_from_content_and_are_capped() {
        let mut table = TableState::new();
        table.set_viewport(10);
        let long = "x".repeat(100);
        table.set_page(page(&[("u1", &[long.as_str(), "1/1"])]));
        let header = table.header_line();
        assert!(header.starts_with("NAME"));
        let (selected, line) = table.visible_lines().remove(0);
        assert!(selected);
        assert!(line.contains('\u{2026}'), "over-cap cells truncate: {line}");
        assert!(
            line.chars().count() <= MAX_COL + 2 + 5,
            "width is bounded: {}",
            line.len()
        );
    }

    #[test]
    fn a_continuation_page_appends_behind_the_held_rows_and_keeps_the_selection() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(numbered_page(0..3, Some("tok-2")));
        assert_eq!(table.continue_token(), Some("tok-2"));
        table.move_selection(2);
        assert_eq!(table.selected_row().unwrap().uid, "u2");

        table.append_page(numbered_page(3..6, None));
        assert_eq!(table.total_rows(), 6);
        assert_eq!(
            table.selected_row().unwrap().uid,
            "u2",
            "loading more must not move the selection"
        );
        assert_eq!(table.continue_token(), None, "the new page's token wins");
        assert!(!table.truncated());

        table.set_page(numbered_page(0..2, None));
        assert_eq!(table.total_rows(), 2, "a refetch starts over");
    }

    #[test]
    fn the_row_cap_bounds_what_the_table_holds_and_says_so() {
        let mut table = TableState::new();
        table.set_viewport(10);
        table.set_page(numbered_page(0..MAX_ROWS - 1, Some("tok")));
        assert!(!table.capped());

        table.append_page(numbered_page(MAX_ROWS - 1..MAX_ROWS + 5, Some("tok-2")));
        assert_eq!(table.total_rows(), MAX_ROWS);
        assert!(table.capped());
        assert_eq!(
            table.continue_token(),
            None,
            "past the cap there is no next page to offer"
        );
    }

    #[test]
    fn paging_keeps_the_selection_on_screen() {
        let mut table = TableState::new();
        table.set_viewport(5);
        let rows: Vec<(String, Vec<String>)> = (0..20)
            .map(|i| (format!("u{i}"), vec![format!("row-{i}"), "1/1".to_string()]))
            .collect();
        let page = TablePage {
            columns: vec![
                TableColumn {
                    name: "Name".to_string(),
                    wide: false,
                },
                TableColumn {
                    name: "Ready".to_string(),
                    wide: false,
                },
            ],
            rows: rows
                .iter()
                .map(|(uid, cells)| TableRow {
                    cells: cells.clone(),
                    name: cells[0].clone(),
                    namespace: None,
                    uid: uid.clone(),
                })
                .collect(),
            truncated: true,
            continue_token: None,
        };
        table.set_page(page);
        assert!(table.truncated());

        table.page_by(1);
        assert_eq!(table.selected_row().unwrap().uid, "u4");
        table.page_by(1);
        assert_eq!(table.selected_row().unwrap().uid, "u8");
        let lines = table.visible_lines();
        assert_eq!(lines.len(), 5);
        assert!(lines.iter().any(|(selected, _)| *selected));

        table.select_last();
        assert_eq!(table.selected_row().unwrap().uid, "u19");
        table.select_first();
        assert_eq!(table.selected_row().unwrap().uid, "u0");
        assert_eq!(
            table.visible_lines()[0].1.split_whitespace().next(),
            Some("row-0")
        );

        table.move_selection(-3);
        assert_eq!(table.selected_row().unwrap().uid, "u0", "clamped");
    }
}
