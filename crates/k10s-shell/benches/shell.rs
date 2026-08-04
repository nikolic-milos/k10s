//! The shell's file-facing and render-facing state machines, benched headless.
//!
//! These are the paths a person waits on that the editor-engine suite cannot
//! see: reading an opened folder into a tree, filtering a directory listing on
//! a keystroke, scoring a fuzzy file query, and composing one viewport's worth
//! of styled runs. Every one of them used to run on the UI thread or rescan its
//! whole input per row, so the numbers here are the ones that say whether a
//! keystroke still fits in a frame. The filesystem is the fake one -- no disk,
//! no clocks -- so the measurement is the state machine and nothing else. Work
//! counters (entries, rows, matches, spans) are deterministic and gated exactly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use k10s_bench::{Config, Samples, measure};
use k10s_edit::complete::{DiagnosticSeverity, validate};
use k10s_edit::{Buffer, Diagnostic, SchemaIndex, SearchState, Selection, Syntax};
use k10s_shell::diff::{DiffState, Mode};
use k10s_shell::editor::{compose_line, viewport_layers};
use k10s_shell::files::{FilesState, read_tree};
use k10s_shell::finder::{PickerMode, PickerState, rank, scan_root};
use k10s_shell::fs::Fs;
use k10s_shell::fs::fake::FakeFs;

// One 60 Hz frame: what a keystroke's share of the work has to fit inside. The
// cases a person waits on are gated against it absolutely, not only against the
// last recording, because a ratio gate accepts whatever the last recording was.
const FRAME_NS: f64 = 16_666_667.0;

const FAST: Config = Config::new(100, 100, 200_000, Duration::from_millis(120));
const SLOW: Config = Config::new(3, 20, 2_000, Duration::from_millis(300));

const TREE_FILES: usize = 5_000;
const FLAT_ENTRIES: usize = 2_000;
const VIEWPORT_ROWS: usize = 60;
const DOCUMENT: usize = 256 << 10;

struct Row {
    scenario: &'static str,
    op: &'static str,
    entries: usize,
    result: usize,
    samples: Samples,
}

impl Row {
    fn budget(&self) -> Option<f64> {
        matches!(
            self.op,
            "keystroke" | "layers" | "query-and-compose" | "diff-paint" | "diff-fold"
        )
        .then_some(FRAME_NS)
    }
}

fn check_budgets(rows: &[Row]) {
    let mut over = Vec::new();
    for row in rows {
        let Some(budget) = row.budget() else {
            continue;
        };
        let measured = row.samples.percentile(0.50);
        if measured > budget {
            over.push(format!(
                "{} {}: p50 {:.3} ms does not fit a {:.1} ms frame",
                row.scenario,
                row.op,
                measured / 1e6,
                budget / 1e6
            ));
        }
    }
    if over.is_empty() {
        return;
    }
    eprintln!("interactive work outside its frame budget:");
    for line in &over {
        eprintln!("    {line}");
    }
    std::process::exit(1);
}

// A workspace shaped like one somebody keeps manifests in: a base directory,
// overlays per environment, and a chart tree, deep enough that expansion is
// recursive work rather than one listing.
fn tree_paths() -> Vec<String> {
    let mut paths = Vec::with_capacity(TREE_FILES);
    let mut index = 0usize;
    while paths.len() < TREE_FILES {
        let environment = index % 8;
        let service = (index / 8) % 25;
        let file = index % 7;
        paths.push(format!(
            "/work/overlays/env-{environment}/service-{service}/resource-{file}.yaml"
        ));
        paths.push(format!("/work/base/service-{service}/resource-{file}.yaml"));
        index += 1;
    }
    paths.truncate(TREE_FILES);
    paths.push("/work/README.md".to_string());
    paths
}

fn tree_fs(paths: &[String]) -> FakeFs {
    let seeded: Vec<(&str, &str)> = paths.iter().map(|path| (path.as_str(), "x")).collect();
    FakeFs::with_files(&seeded)
}

// One flat directory, which is what a picker filters on every keystroke.
fn flat_fs() -> FakeFs {
    let names: Vec<String> = (0..FLAT_ENTRIES)
        .map(|index| format!("/work/flat/deployment-{index:05}.yaml"))
        .collect();
    let seeded: Vec<(&str, &str)> = names.iter().map(|path| (path.as_str(), "x")).collect();
    FakeFs::with_files(&seeded)
}

fn manifest(target_bytes: usize) -> String {
    let mut out = String::from(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: bench\nspec:\n  template:\n    spec:\n      containers:\n",
    );
    let mut index = 0usize;
    while out.len() < target_bytes {
        out.push_str(&format!(
            "        - name: worker-{index}\n          image: registry.example.com/app:1.{index}\n          imagePullPolicy: IfNotPresent\n"
        ));
        index += 1;
    }
    out
}

fn file_tree(rows: &mut Vec<Row>) {
    let paths = tree_paths();
    let fs = tree_fs(&paths);
    let root = PathBuf::from("/work");
    let mut state = FilesState::default();
    state.set_viewport(40);
    // Half the environments left open, which is what a person actually
    // navigates with: the read is recursive from the first row.
    for environment in 0..4 {
        state
            .expanded
            .insert(PathBuf::from("/work/overlays".to_string()));
        state
            .expanded
            .insert(PathBuf::from(format!("/work/overlays/env-{environment}")));
    }
    let mut listed = 0usize;
    let samples = measure(SLOW, || {
        listed = read_tree(&root, &state.expanded, &fs).rows.len();
    });
    rows.push(Row {
        scenario: "tree-5k",
        op: "read",
        entries: paths.len(),
        result: listed,
        samples,
    });

    let samples = measure(FAST, || {
        let listing = read_tree(&root, &state.expanded, &fs);
        state.apply(listing);
    });
    rows.push(Row {
        scenario: "tree-5k",
        op: "read-and-apply",
        entries: paths.len(),
        result: state.rows.len(),
        samples,
    });
}

fn picker(rows: &mut Vec<Row>) {
    let fs = flat_fs();
    let mut state = PickerState::new(Path::new("/work/flat"), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    let listed = fs.list_dir(Path::new(&dir));
    assert!(state.listed(&dir, listed), "the listing is the one wanted");
    let entries = state.entries.len();
    // A keystroke inside a listed directory: filtering only, which is the whole
    // point of listing once.
    state.input = "/work/flat/deployment-007".to_string();
    let mut matched = 0usize;
    let samples = measure(FAST, || {
        state.refilter();
        matched = state.matches.len();
    });
    rows.push(Row {
        scenario: "dir-2k",
        op: "keystroke",
        entries,
        result: matched,
        samples,
    });
}

fn finder(rows: &mut Vec<Row>) {
    let paths = tree_paths();
    let fs = tree_fs(&paths);
    let mut scanned = 0usize;
    let samples = measure(SLOW, || {
        scanned = scan_root(&fs, Path::new("/work")).files.len();
    });
    rows.push(Row {
        scenario: "scan-5k",
        op: "scan",
        entries: paths.len(),
        result: scanned,
        samples,
    });

    let scan = scan_root(&fs, Path::new("/work"));
    let mut matched = 0usize;
    let samples = measure(FAST, || {
        matched = rank(&scan.files, "envsvcresyml").0.len();
    });
    rows.push(Row {
        scenario: "scan-5k",
        op: "keystroke",
        entries: scan.files.len(),
        result: matched,
        samples,
    });
}

// The shipped render path: what one frame of visible rows costs when the
// document is large, the search is showing thousands of matches, the validator
// has filled its diagnostic budget, and the user is holding a column of
// cursors.
fn viewport(rows: &mut Vec<Row>) {
    let text = manifest(DOCUMENT);
    let mut buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let rope = buffer.rope();

    let mut search = SearchState::new("image", false);
    search.refresh(&buffer);
    let matches: Vec<std::ops::Range<usize>> = search.matches().to_vec();

    let index = SchemaIndex::new();
    let mut diagnostics: Vec<Diagnostic> = validate(&index, rope, &syntax);
    while diagnostics.len() < 200 {
        let at = rope.len() / 2 + diagnostics.len() * 4;
        diagnostics.push(Diagnostic {
            range: at..at + 3,
            severity: DiagnosticSeverity::Warning,
            message: String::new(),
        });
    }

    let first = rope.len_lines() / 2;
    let last = (first + VIEWPORT_ROWS).min(rope.len_lines());
    let selections: Vec<Selection> = (first..last)
        .step_by(2)
        .map(|row| Selection::caret(rope.line_start(row) + 4))
        .collect();
    buffer.set_selections(selections, 0);

    let rope = buffer.rope();
    let bytes = rope.line_start(first)..rope.line_start(last - 1) + rope.line_len(last - 1);
    let tokens = syntax.highlights(rope, bytes.clone());
    let cursors = buffer.selections().len();

    let mut produced = 0usize;
    let samples = measure(FAST, || {
        let layers = viewport_layers(
            rope,
            &tokens,
            buffer.selections(),
            &matches,
            matches.first(),
            &diagnostics,
            first..last,
        );
        produced = layers.iter().map(|row| row.tokens.len()).sum();
    });
    rows.push(Row {
        scenario: "viewport-60",
        op: "layers",
        entries: matches.len(),
        result: produced,
        samples,
    });

    // The whole frame, query included, which is what render actually calls.
    let mut spans = 0usize;
    let samples = measure(FAST, || {
        let tokens = syntax.highlights(rope, bytes.clone());
        let layers = viewport_layers(
            rope,
            &tokens,
            buffer.selections(),
            &matches,
            matches.first(),
            &diagnostics,
            first..last,
        );
        spans = layers
            .iter()
            .enumerate()
            .map(|(offset, row)| compose_line(rope.line_len(first + offset) + 1, row).len())
            .sum();
    });
    rows.push(Row {
        scenario: "viewport-60",
        op: "query-and-compose",
        entries: cursors,
        result: spans,
        samples,
    });
}

// What a visible diff costs per frame, and what folding one costs on a keypress.
// The comparison itself is the editor engine's to measure; what belongs here is
// the projection -- picking the sixty rows the viewport shows out of a
// thirty-thousand-row comparison, and rebuilding the row list when the fold
// changes -- because that is the part that runs while a person scrolls.
fn diff_view(rows: &mut Vec<Row>) {
    let live = manifest(DOCUMENT);
    let buffer = live.replacen("app:1.7\n", "app:1.7-hotfix\n", 1);
    assert_ne!(buffer, live, "the fixture has an image tag to edit");
    let mut state = DiffState::new();
    state.set_viewport(VIEWPORT_ROWS);
    state.set(Mode::Local, live, None, buffer);
    // Unfolded, which is both the state a person scrolling a whole manifest is
    // in and the widest the projection ever works over: every line of the
    // document is a row.
    state.toggle_folded();
    assert!(!state.folded(), "the fold starts on");
    let cells = state.len();
    // Off the top, so the paint is over content rather than the first rows.
    state.next_change();

    let mut painted = 0usize;
    let samples_paint = measure(FAST, || {
        painted = state.visible().map(|row| row.rendered().len()).sum();
    });
    rows.push(Row {
        scenario: "diff-256k",
        op: "diff-paint",
        entries: cells,
        result: painted,
        samples: samples_paint,
    });

    // Toggling alternates between the folded and unfolded row lists, so the
    // timing covers both directions; the counter is the widest list seen, which
    // is the unfolded one and is reached on the first iteration whatever the
    // sample count turns out to be.
    let mut widest = 0usize;
    let samples_fold = measure(FAST, || {
        state.toggle_folded();
        widest = widest.max(state.len());
    });
    rows.push(Row {
        scenario: "diff-256k",
        op: "diff-fold",
        entries: cells,
        result: widest,
        samples: samples_fold,
    });
}

fn main() {
    let json = std::env::args().any(|argument| argument == "--json");
    let mut rows = Vec::new();
    file_tree(&mut rows);
    picker(&mut rows);
    finder(&mut rows);
    viewport(&mut rows);
    diff_view(&mut rows);
    if json {
        print_json(&rows);
    } else {
        print_table(&rows);
    }
    check_budgets(&rows);
}

fn print_table(rows: &[Row]) {
    println!(
        "{:<14} {:<20} {:>9} {:>8} {:>12} {:>12} {:>8}",
        "scenario", "op", "entries", "result", "p50_ns", "p99_ns", "samples"
    );
    for row in rows {
        println!(
            "{:<14} {:<20} {:>9} {:>8} {:>12.0} {:>12.0} {:>8}",
            row.scenario,
            row.op,
            row.entries,
            row.result,
            row.samples.percentile(0.50),
            row.samples.percentile(0.99),
            row.samples.sample_count(),
        );
    }
}

fn print_json(rows: &[Row]) {
    println!("{{");
    println!("  \"schema_version\": 1,");
    println!("  \"mode\": \"shell\",");
    println!("  \"cases\": [");
    for (index, row) in rows.iter().enumerate() {
        let comma = if index + 1 == rows.len() { "" } else { "," };
        println!("    {{");
        println!("      \"scenario\": \"{}\",", row.scenario);
        println!("      \"op\": \"{}\",", row.op);
        println!("      \"entries\": {},", row.entries);
        println!("      \"result\": {},", row.result);
        println!("      \"iters\": {},", row.samples.iterations());
        println!("      \"samples\": {},", row.samples.sample_count());
        println!("      \"batch_size\": {},", row.samples.batch_size());
        println!("      \"p50_rmad\": {:.6},", row.samples.p50_relative_mad());
        println!("      \"p50_ns\": {:.3},", row.samples.percentile(0.50));
        println!("      \"p99_ns\": {:.3}", row.samples.percentile(0.99));
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}
