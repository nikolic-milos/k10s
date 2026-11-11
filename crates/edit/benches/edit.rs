//! The editor engine benched headless: what a keystroke actually costs.
//!
//! Deterministic synthetic manifests at three sizes drive the paths a person
//! sits inside: a single-cursor keystroke with its incremental reparse, the
//! same keystroke under 64 cursors, a from-scratch parse (open and undo pay
//! it), viewport highlighting, a regex search including its compile, schema
//! completion at a container-keys position, whole-document validation, the
//! three-way and two-way diffs a person waits through after asking what an
//! apply would change, and the payload prune that precedes one.
//! Work counters (bytes, lines, per-op results) are deterministic and meant
//! for exact gating; timings emit p50/p99 with sample counts so the
//! comparator can refuse a thin p99.
//!
//! The interactive cases also carry an absolute budget, not just a baseline. A
//! comparator that only checks the ratio to the last recording accepts whatever
//! the last recording happened to be -- which is how a 33 ms completion sat in
//! a gate that passed -- so anything a person waits on inside a keystroke is
//! measured against one 60 Hz frame here, and the suite fails if it does not
//! fit. Whole-document work that is deliberately debounced or asynchronous
//! carries no budget, and says so by carrying none.

use std::time::Duration;

use k10s_bench::{Config, Samples, measure};
use k10s_edit::complete::{complete, doc_meta, validate};
use k10s_edit::{Buffer, SchemaIndex, SearchState, Selection, Sides, Syntax, three_way};

const FAST: Config = Config::new(100, 100, 200_000, Duration::from_millis(120));
const SLOW: Config = Config::new(3, 20, 2_000, Duration::from_millis(300));

// One 60 Hz frame. A keystroke's work happens between two of them.
const FRAME_NS: f64 = 16_666_667.0;

const SMALL: usize = 16 << 10;
const MEDIUM: usize = 256 << 10;
const LARGE: usize = 1 << 20;

const BENCH_SCHEMA: &str = r##"{
  "openapi": "3.0.0",
  "components": { "schemas": {
    "io.k8s.api.apps.v1.Deployment": {
      "type": "object",
      "properties": {
        "apiVersion": { "type": "string" },
        "kind": { "type": "string" },
        "metadata": { "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta" },
        "spec": { "$ref": "#/components/schemas/io.k8s.api.apps.v1.DeploymentSpec" }
      },
      "x-kubernetes-group-version-kind": [{ "group": "apps", "version": "v1", "kind": "Deployment" }]
    },
    "io.k8s.api.apps.v1.DeploymentSpec": {
      "type": "object",
      "required": ["selector", "template"],
      "properties": {
        "replicas": { "type": "integer" },
        "selector": { "type": "object" },
        "template": { "$ref": "#/components/schemas/io.k8s.api.core.v1.PodTemplateSpec" }
      }
    },
    "io.k8s.api.core.v1.PodTemplateSpec": {
      "type": "object",
      "properties": {
        "spec": { "$ref": "#/components/schemas/io.k8s.api.core.v1.PodSpec" }
      }
    },
    "io.k8s.api.core.v1.PodSpec": {
      "type": "object",
      "required": ["containers"],
      "properties": {
        "containers": { "type": "array", "items": { "$ref": "#/components/schemas/io.k8s.api.core.v1.Container" } }
      }
    },
    "io.k8s.api.core.v1.Container": {
      "type": "object",
      "required": ["name"],
      "properties": {
        "name": { "type": "string", "description": "Name of the container." },
        "image": { "type": "string", "description": "Container image name." },
        "imagePullPolicy": { "type": "string", "enum": ["Always", "Never", "IfNotPresent"] },
        "ports": { "type": "array", "items": { "type": "object" } }
      }
    },
    "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta": {
      "type": "object",
      "properties": {
        "name": { "type": "string" },
        "labels": { "type": "object", "additionalProperties": { "type": "string" } }
      }
    }
  } }
}"##;

fn manifest(target_bytes: usize) -> String {
    let mut out = String::from(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: bench\n  labels:\n    app: bench\nspec:\n  replicas: 3\n  selector: {}\n  template:\n    spec:\n      containers:\n",
    );
    let mut index = 0usize;
    while out.len() < target_bytes {
        out.push_str(&format!(
            "        - name: worker-{index}\n          image: registry.example.com/app:1.{index}\n          imagePullPolicy: IfNotPresent\n          ports:\n            - containerPort: {}\n",
            8000 + (index % 1000)
        ));
        index += 1;
    }
    out
}

// The same document as JSON: the editor's other grammar, and the shape the
// settings and keymap files are.
fn json_manifest(target_bytes: usize) -> String {
    let mut out = String::from(
        "{\n  \"apiVersion\": \"apps/v1\",\n  \"kind\": \"Deployment\",\n  \"metadata\": { \"name\": \"bench\" },\n  \"spec\": {\n    \"replicas\": 3,\n    \"template\": {\n      \"spec\": {\n        \"containers\": [\n",
    );
    let mut index = 0usize;
    while out.len() < target_bytes {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "          {{ \"name\": \"worker-{index}\", \"image\": \"registry.example.com/app:1.{index}\", \"imagePullPolicy\": \"IfNotPresent\" }}"
        ));
        index += 1;
    }
    out.push_str("\n        ]\n      }\n    }\n  }\n}\n");
    out
}

// A manifest as the cluster serves it, which unlike a hand-written one carries
// the fields the API server owns: what the apply payload has to take out, and
// what the editor's document has in it while a diff runs over it.
fn fetched_manifest(target_bytes: usize) -> String {
    let body = manifest(target_bytes);
    let (head, rest) = body
        .split_once("spec:\n")
        .expect("the fixture opens with metadata and then a spec");
    format!(
        "{head}  creationTimestamp: \"2026-08-02T10:00:00Z\"\n  resourceVersion: \"48210\"\n  uid: 0f2c-1cc9\nspec:\n{rest}status:\n  observedGeneration: 3\n  replicas: 3\n"
    )
}

// The three documents a three-way diff is actually handed. The base is what was
// declared -- without the pull policy the server defaulted in -- so base and
// live differ once per container, which is the shape that makes the alignment
// work rather than trim; the buffer then changes one image tag.
fn diff_sides(target_bytes: usize) -> (String, String, String) {
    let live = fetched_manifest(target_bytes);
    let base: String = live
        .lines()
        .filter(|line| !line.trim_start().starts_with("imagePullPolicy:"))
        .flat_map(|line| [line, "\n"])
        .collect();
    let buffer = edited(&live);
    (base, live, buffer)
}

fn edited(live: &str) -> String {
    let changed = live.replacen("app:1.7\n", "app:1.7-hotfix\n", 1);
    assert_ne!(changed, live, "the fixture has an image tag to edit");
    changed
}

fn schema_index() -> SchemaIndex {
    let mut index = SchemaIndex::new();
    index
        .add_openapi_document(BENCH_SCHEMA)
        .expect("the bench schema parses");
    index.add_api_version("v1");
    index
}

fn size_label(bytes: usize) -> &'static str {
    match bytes {
        SMALL => "16k",
        MEDIUM => "256k",
        LARGE => "1m",
        _ => unreachable!("every scenario is named"),
    }
}

// Where the cursor is, mid-keystroke, on a broken parse: the answer completion
// waits for before it can rank anything. Measured on its own because it is the
// part that used to scan the whole document.
fn cursor_context(target: usize, rows: &mut Vec<Row>) {
    let mut text = manifest(target);
    text.push_str("        - name: extra\n          im");
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let offset = buffer.rope().len();
    let mut depth = 0usize;
    let samples = measure(FAST, || {
        depth = syntax.context_at(buffer.rope(), offset).path.len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "cursor-context",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: depth,
        samples,
    });
}

fn json_label(bytes: usize) -> &'static str {
    match bytes {
        SMALL => "json-16k",
        MEDIUM => "json-256k",
        _ => unreachable!("every scenario is named"),
    }
}

// The JSON side of the engine: the same keystroke and whole-document work the
// YAML cases measure, on the grammar the settings and keymap files use.
fn json_cases(target: usize, rows: &mut Vec<Row>) {
    let text = json_manifest(target);
    let mut buffer = Buffer::new(&text);
    let mut syntax = Syntax::json();
    syntax.reparse(buffer.rope());
    let rope = buffer.rope();
    let image_row = (0..rope.len_lines())
        .find(|row| rope.line(*row).contains("\"image\""))
        .expect("the fixture has image fields");
    let caret = rope.line_start(image_row) + rope.line_len(image_row) - 2;
    buffer.set_selections(vec![Selection::caret(caret)], 0);
    let bytes = buffer.rope().len();
    let lines = buffer.rope().len_lines();
    let mut inserted = false;
    let samples = measure(FAST, || {
        let splices = if inserted {
            buffer.backspace()
        } else {
            buffer.insert("x")
        };
        inserted = !inserted;
        syntax.edit(buffer.rope(), &splices);
    });
    rows.push(Row {
        scenario: json_label(target),
        op: "keystroke",
        bytes,
        lines,
        result: 1,
        samples,
    });

    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::json();
    let samples = measure(SLOW, || {
        syntax.reparse(buffer.rope());
    });
    rows.push(Row {
        scenario: json_label(target),
        op: "full-parse",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: 1,
        samples,
    });

    let rope = buffer.rope();
    let middle = rope.len_lines() / 2;
    let start = rope.line_start(middle);
    let end = rope.line_start((middle + 60).min(rope.len_lines() - 1));
    let mut spans = 0usize;
    let samples = measure(FAST, || {
        spans = syntax.highlights(rope, start..end).len();
    });
    let rope = buffer.rope();
    let deep = (0..rope.len_lines())
        .rev()
        .find(|row| rope.line(*row).contains("\"image\""))
        .expect("the fixture has image fields");
    let at = rope.line_start(deep) + rope.line_len(deep) - 2;
    let mut depth = 0usize;
    let samples_context = measure(FAST, || {
        depth = syntax.context_at(rope, at).path.len();
    });
    rows.push(Row {
        scenario: json_label(target),
        op: "highlight-viewport",
        bytes: rope.len(),
        lines: rope.len_lines(),
        result: spans,
        samples,
    });
    rows.push(Row {
        scenario: json_label(target),
        op: "cursor-context",
        bytes: rope.len(),
        lines: rope.len_lines(),
        result: depth,
        samples: samples_context,
    });
}

struct Row {
    scenario: &'static str,
    op: &'static str,
    bytes: usize,
    lines: usize,
    result: usize,
    samples: Samples,
}

impl Row {
    // What the case must fit inside, when it is one a person waits on.
    fn budget(&self) -> Option<f64> {
        if matches!(
            self.op,
            "keystroke" | "keystroke-64" | "cursor-context" | "completion" | "highlight-viewport"
        ) {
            return Some(FRAME_NS);
        }
        // A diff and its payload are what a person waits through after ctrl-s.
        // Every size is budgeted, including the megabyte one: measured, the
        // three-way diff of a 31,000-line document lands near 6 ms, so a frame
        // is a real ceiling here rather than an aspiration -- unlike the
        // from-scratch parse beside it, which does not fit and says so by
        // carrying no budget.
        matches!(self.op, "diff-three-way" | "diff-two-way" | "apply-payload").then_some(FRAME_NS)
    }
}

// The absolute half of the gate. Reported after the numbers so the numbers are
// still readable, and a failure exits non-zero so a collection run stops rather
// than recording a baseline that enshrines the regression.
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

fn keystroke(target: usize, cursors: usize, rows: &mut Vec<Row>) {
    let text = manifest(target);
    let mut buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    // Carets land inside image tag scalars: a value-position keystroke, the
    // shape typing actually has, and one that must stay incremental. A
    // structure-breaking keystroke costs a recovery reparse by design.
    let rope = buffer.rope();
    let image_rows: Vec<usize> = (0..rope.len_lines())
        .filter(|row| rope.line(*row).trim_start().starts_with("image:"))
        .collect();
    let stride = (image_rows.len() / cursors.max(1)).max(1);
    let selections: Vec<Selection> = (0..cursors)
        .map(|cursor| {
            let row = image_rows[(cursor * stride).min(image_rows.len() - 1)];
            Selection::caret(rope.line_start(row) + rope.line_len(row))
        })
        .collect();
    buffer.set_selections(selections, 0);
    let bytes = buffer.rope().len();
    let lines = buffer.rope().len_lines();
    let mut inserted = false;
    let samples = measure(FAST, || {
        let splices = if inserted {
            buffer.backspace()
        } else {
            buffer.insert("x")
        };
        inserted = !inserted;
        syntax.edit(buffer.rope(), &splices);
    });
    rows.push(Row {
        scenario: size_label(target),
        op: if cursors == 1 {
            "keystroke"
        } else {
            "keystroke-64"
        },
        bytes,
        lines,
        result: cursors,
        samples,
    });
}

fn full_parse(target: usize, rows: &mut Vec<Row>) {
    let text = manifest(target);
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    let samples = measure(SLOW, || {
        syntax.reparse(buffer.rope());
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "full-parse",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: 1,
        samples,
    });
}

fn highlight_viewport(target: usize, rows: &mut Vec<Row>) {
    let text = manifest(target);
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let rope = buffer.rope();
    let middle = rope.len_lines() / 2;
    let start = rope.line_start(middle);
    let end = rope.line_start((middle + 60).min(rope.len_lines() - 1));
    let mut spans = 0usize;
    let samples = measure(FAST, || {
        spans = syntax.highlights(rope, start..end).len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "highlight-viewport",
        bytes: rope.len(),
        lines: rope.len_lines(),
        result: spans,
        samples,
    });
}

fn search_regex(target: usize, rows: &mut Vec<Row>) {
    let text = manifest(target);
    let buffer = Buffer::new(&text);
    let mut found = 0usize;
    let samples = measure(SLOW, || {
        let mut search = SearchState::new(r"image: registry\.[a-z.]+/app:1\.\d+", true);
        search.refresh(&buffer);
        found = search.matches().len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "search-regex",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: found,
        samples,
    });
}

fn completion(target: usize, rows: &mut Vec<Row>) {
    let mut text = manifest(target);
    text.push_str("        - name: extra\n          im");
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let index = schema_index();
    let offset = buffer.rope().len();
    let mut count = 0usize;
    let samples = measure(FAST, || {
        let context = syntax.context_at(buffer.rope(), offset);
        let meta = doc_meta(buffer.rope(), &syntax, context.document_index);
        let existing = syntax.mapping_keys_at(buffer.rope(), context.document_index, &context.path);
        count = complete(&index, &meta, &context, &existing).len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "completion",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: count,
        samples,
    });
}

fn validation(target: usize, rows: &mut Vec<Row>) {
    let text = manifest(target);
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let index = schema_index();
    let mut count = 0usize;
    let samples = measure(SLOW, || {
        count = validate(&index, buffer.rope(), &syntax).len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "validate",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: count,
        samples,
    });
}

// The comparison the editor shows when asked what an apply would change: live,
// last-applied and the buffer, aligned and classified.
fn diff_three_way(target: usize, rows: &mut Vec<Row>) {
    let (base, live, buffer) = diff_sides(target);
    let mut count = 0usize;
    let samples = measure(FAST, || {
        count = three_way(Sides {
            base: Some(&base),
            live: &live,
            buffer: &buffer,
        })
        .rows
        .len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "diff-three-way",
        bytes: live.len(),
        lines: live.lines().count(),
        result: count,
        samples,
    });
}

// The comparison a dry run produces: two documents, one of them the server's own
// answer. This is the one on the ctrl-s path.
fn diff_two_way(target: usize, rows: &mut Vec<Row>) {
    let live = fetched_manifest(target);
    let would_be = edited(&live);
    let mut count = 0usize;
    let samples = measure(FAST, || {
        count = three_way(Sides {
            base: None,
            live: &live,
            buffer: &would_be,
        })
        .rows
        .len();
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "diff-two-way",
        bytes: live.len(),
        lines: live.lines().count(),
        result: count,
        samples,
    });
}

// Building what the apply sends: every server-owned field located on the tree
// and spliced out of the document's own bytes.
fn apply_payload(target: usize, rows: &mut Vec<Row>) {
    let text = fetched_manifest(target);
    let buffer = Buffer::new(&text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let mut bytes = 0usize;
    let samples = measure(FAST, || {
        bytes = k10s_edit::apply::payload(buffer.rope(), &syntax, 0, true)
            .sendable()
            .map_or(0, str::len);
    });
    rows.push(Row {
        scenario: size_label(target),
        op: "apply-payload",
        bytes: buffer.rope().len(),
        lines: buffer.rope().len_lines(),
        result: bytes,
        samples,
    });
}

fn main() {
    let json = std::env::args().any(|argument| argument == "--json");
    let mut rows = Vec::new();
    for target in [SMALL, MEDIUM, LARGE] {
        keystroke(target, 1, &mut rows);
    }
    keystroke(SMALL, 64, &mut rows);
    keystroke(MEDIUM, 64, &mut rows);
    full_parse(SMALL, &mut rows);
    full_parse(MEDIUM, &mut rows);
    highlight_viewport(MEDIUM, &mut rows);
    highlight_viewport(LARGE, &mut rows);
    search_regex(MEDIUM, &mut rows);
    search_regex(LARGE, &mut rows);
    completion(SMALL, &mut rows);
    completion(MEDIUM, &mut rows);
    completion(LARGE, &mut rows);
    cursor_context(MEDIUM, &mut rows);
    cursor_context(LARGE, &mut rows);
    validation(SMALL, &mut rows);
    validation(MEDIUM, &mut rows);
    json_cases(SMALL, &mut rows);
    json_cases(MEDIUM, &mut rows);
    for target in [SMALL, MEDIUM, LARGE] {
        diff_three_way(target, &mut rows);
        diff_two_way(target, &mut rows);
    }
    for target in [SMALL, MEDIUM, LARGE] {
        apply_payload(target, &mut rows);
    }
    if json {
        print_json(&rows);
    } else {
        print_table(&rows);
    }
    check_budgets(&rows);
}

fn print_table(rows: &[Row]) {
    println!(
        "{:<10} {:<20} {:>9} {:>8} {:>8} {:>12} {:>12} {:>8}",
        "scenario", "op", "bytes", "lines", "result", "p50_ns", "p99_ns", "samples"
    );
    for row in rows {
        println!(
            "{:<10} {:<20} {:>9} {:>8} {:>8} {:>12.0} {:>12.0} {:>8}",
            row.scenario,
            row.op,
            row.bytes,
            row.lines,
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
    println!("  \"mode\": \"editor\",");
    println!("  \"cases\": [");
    for (index, row) in rows.iter().enumerate() {
        let comma = if index + 1 == rows.len() { "" } else { "," };
        println!("    {{");
        println!("      \"scenario\": \"{}\",", row.scenario);
        println!("      \"op\": \"{}\",", row.op);
        println!("      \"bytes\": {},", row.bytes);
        println!("      \"lines\": {},", row.lines);
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
