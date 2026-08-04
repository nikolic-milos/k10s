//! Document structure: incrementally reparsed tree-sitter trees behind one
//! language seam.
//!
//! Every buffer transaction feeds its splices to [`Syntax::edit`] in
//! application order, so the old tree's coordinates are valid at each
//! `Tree::edit` call and the reparse is proportional to the change; undo
//! and redo reparse from scratch. Highlights come from each grammar's own
//! bundled query mapped onto a small [`TokenKind`] set with nested captures
//! split rather than dropped. Adding a language means one [`LanguageKind`]
//! arm, its node-kind tables, and its capture names -- the walkers are
//! shared. YAML multi-document streams are first-class; JSON's root is its
//! one document. [`Syntax::context_at`] derives the cursor's mapping path for
//! completion, and because completion happens mid-keystroke on exactly the
//! text a parser rejects, every language carries a text-only fallback for
//! when the tree knows less than the characters do: indentation for YAML,
//! a backward scan over unclosed brackets for JSON. JSON also reads its
//! key-or-value position from the tree rather than from the line, because its
//! values are quoted strings that routinely contain colons. Plain text parses
//! nothing and answers everything with graceful empties.

use crate::buffer::Splice;
use crate::rope::Rope;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{InputEdit, Node, Parser, Query, QueryCursor, Tree};

const MAX_ERROR_RANGES: usize = 200;
// How much text the backward scans materialize at a time. Both the YAML indent
// walk and the JSON bracket walk are bounded by the document, and neither may
// copy it whole: a keystroke's completion is inside a frame or it is not
// completion.
const SCAN_WINDOW: usize = 8 << 10;

// The document ranges of one parse, shared by every caller that asks for them
// in the same keystroke, held alongside the rope length they describe.
type CachedDocuments = Option<(usize, Arc<Vec<Range<usize>>>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageKind {
    Yaml,
    Json,
    Plain,
}

impl LanguageKind {
    pub fn from_file_name(name: &str) -> LanguageKind {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".yaml") || lower.ends_with(".yml") {
            LanguageKind::Yaml
        } else if lower.ends_with(".json") {
            LanguageKind::Json
        } else {
            LanguageKind::Plain
        }
    }
}

// The scalar shape a tree node claims, normalized across grammars so
// validation compares semantics rather than node-kind strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarClass {
    Str,
    Int,
    Float,
    Bool,
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Property,
    Str,
    Number,
    Boolean,
    Constant,
    Comment,
    Anchor,
    Tag,
    Directive,
    Punctuation,
    PunctuationSpecial,
}

fn capture_kind(name: &str) -> Option<TokenKind> {
    match name {
        "property" | "string.special.key" => Some(TokenKind::Property),
        "string" => Some(TokenKind::Str),
        "number" => Some(TokenKind::Number),
        "boolean" => Some(TokenKind::Boolean),
        "constant.builtin" | "escape" => Some(TokenKind::Constant),
        "comment" => Some(TokenKind::Comment),
        "label" => Some(TokenKind::Anchor),
        "type" => Some(TokenKind::Tag),
        "attribute" => Some(TokenKind::Directive),
        "punctuation.delimiter" | "punctuation.bracket" => Some(TokenKind::Punctuation),
        "punctuation.special" => Some(TokenKind::PunctuationSpecial),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    Key(String),
    Index(usize),
}

/// One `key: value` pair located by path, with the size of the mapping it sits
/// in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub bytes: Range<usize>,
    /// How many *entries* the mapping holds. A comment is an extra, not an
    /// entry, and counting one told a caller a mapping it was about to empty
    /// still had something in it.
    pub siblings: usize,
}

/// What a path resolved to in one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Pair(Pair),
    /// A key on the path -- its last segment, or a mapping on the way to it --
    /// is written more than once in the mapping that holds it. Resolution here
    /// takes the first copy and nothing says the API server takes the same one,
    /// so this is not "found" and it is not "absent": it is a document a caller
    /// editing by byte range cannot act on.
    Repeated,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPosition {
    Key,
    Value,
}

// Where a path walk ended up: the node and whatever is left of the document
// scope, or the fact that the walk reached a fork it cannot choose at.
enum Descent<'tree> {
    At(Node<'tree>, Option<Range<usize>>),
    Repeated,
    Absent,
}

// What a walk does about a key written twice in one mapping. The apply pruner
// has to stop: it is about to delete bytes, and nothing says the API server
// resolves the copy this walk did. Everyone else takes the first one and stops
// scanning there, because a cursor lookup runs on a keystroke and reading a
// whole mapping to rule out a duplicate nobody will act on is the expensive
// answer to a cheap question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnRepeat {
    Stop,
    TakeFirst,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorContext {
    pub path: Vec<PathSeg>,
    pub prefix: String,
    pub position: CursorPosition,
    pub value_key: Option<String>,
    pub document_index: usize,
    pub language: LanguageKind,
    // Where the cursor sits relative to a quoted string, as the tree sees it.
    pub string_site: StringSite,
}

// The character before an empty prefix is a quote whether it opened the string
// or closed it, and the difference is whether an insertion replaces that string
// or eats the previous value's terminator. Only the tree can tell them apart --
// and only when it has a string node there at all, which it does not for a
// quote the user has only just typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringSite {
    // Inside a string, including one the grammar could not terminate.
    Inside,
    // Immediately after a string carrying both of its quotes.
    After,
    // No string node touches the cursor; the text is all there is to go on.
    Outside,
}

struct Engine {
    parser: Parser,
    query: Query,
    capture_kinds: Vec<Option<TokenKind>>,
}

impl Engine {
    fn load(language: tree_sitter::Language, highlights: &str) -> Engine {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .expect("the bundled grammar matches the linked tree-sitter");
        let query = Query::new(&language, highlights)
            .expect("the grammar's own highlight query compiles against it");
        let capture_kinds = query
            .capture_names()
            .iter()
            .map(|name| capture_kind(name))
            .collect();
        Engine {
            parser,
            query,
            capture_kinds,
        }
    }
}

pub struct Syntax {
    language: LanguageKind,
    engine: Option<Engine>,
    tree: Option<Tree>,
    // Document ranges are asked for several times per keystroke -- completion
    // needs the cursor's document, `doc_meta` resolves two paths, validation
    // walks each one -- and on a recovered parse deriving them means scanning
    // the buffer for markers. Computed once per parse and dropped whenever the
    // tree moves, which is what makes the answer current: `reparse` and `edit`
    // are the only two ways the tree changes and both clear this first. The
    // length is a cheap mismatch check on top, not a proof -- a rope the tree
    // does not describe but whose length happens to agree would read the stale
    // answer -- so every caller passes the rope the tree was built from.
    documents: RefCell<CachedDocuments>,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Syntax")
            .field("language", &self.language)
            .field("parsed", &self.tree.is_some())
            .finish()
    }
}

impl Syntax {
    pub fn new(language: LanguageKind) -> Syntax {
        let engine = match language {
            LanguageKind::Yaml => Some(Engine::load(
                tree_sitter::Language::from(tree_sitter_yaml::LANGUAGE),
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
            )),
            LanguageKind::Json => Some(Engine::load(
                tree_sitter::Language::from(tree_sitter_json::LANGUAGE),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )),
            LanguageKind::Plain => None,
        };
        Syntax {
            language,
            engine,
            tree: None,
            documents: RefCell::new(None),
        }
    }

    pub fn yaml() -> Syntax {
        Syntax::new(LanguageKind::Yaml)
    }

    pub fn json() -> Syntax {
        Syntax::new(LanguageKind::Json)
    }

    pub fn language(&self) -> LanguageKind {
        self.language
    }

    pub fn reparse(&mut self, rope: &Rope) {
        self.documents.replace(None);
        self.tree = self.run_parser(rope, None);
    }

    pub fn edit(&mut self, rope: &Rope, splices: &[Splice]) {
        self.documents.replace(None);
        let Some(tree) = self.tree.as_mut() else {
            self.reparse(rope);
            return;
        };
        for splice in splices {
            tree.edit(&InputEdit {
                start_byte: splice.start,
                old_end_byte: splice.old_end,
                new_end_byte: splice.new_end,
                start_position: ts_point(splice.start_point),
                old_end_position: ts_point(splice.old_end_point),
                new_end_position: ts_point(splice.new_end_point),
            });
        }
        let old = self.tree.take();
        self.tree = self.run_parser(rope, old.as_ref());
    }

    fn run_parser(&mut self, rope: &Rope, old: Option<&Tree>) -> Option<Tree> {
        self.engine.as_mut()?.parser.parse_with_options(
            &mut |offset, _| rope.chunk_bytes_from(offset),
            old,
            None,
        )
    }

    pub fn is_parsed(&self) -> bool {
        self.tree.is_some()
    }

    pub fn highlights(&self, rope: &Rope, range: Range<usize>) -> Vec<(Range<usize>, TokenKind)> {
        let (Some(tree), Some(engine)) = (&self.tree, &self.engine) else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range);
        let mut captures = cursor.captures(&engine.query, tree.root_node(), |node: Node| {
            rope.chunks_in(node.byte_range()).map(str::as_bytes)
        });
        let mut spans: Vec<(Range<usize>, usize, TokenKind)> = Vec::new();
        while let Some((matched, capture_index)) = captures.next() {
            let capture = matched.captures[*capture_index];
            let Some(kind) = engine.capture_kinds[capture.index as usize] else {
                continue;
            };
            let node_range = capture.node.byte_range();
            if node_range.is_empty() {
                continue;
            }
            spans.push((node_range, matched.pattern_index, kind));
        }
        flatten_spans(spans)
    }

    pub fn document_ranges(&self, rope: &Rope) -> Vec<Range<usize>> {
        self.documents(rope).as_ref().clone()
    }

    fn documents(&self, rope: &Rope) -> Arc<Vec<Range<usize>>> {
        if let Some((length, cached)) = self.documents.borrow().as_ref()
            && *length == rope.len()
        {
            return cached.clone();
        }
        let computed = Arc::new(self.split_documents(rope));
        self.documents.replace(Some((rope.len(), computed.clone())));
        computed
    }

    fn split_documents(&self, rope: &Rope) -> Vec<Range<usize>> {
        let Some(tree) = &self.tree else {
            return std::iter::once(0..rope.len()).collect();
        };
        let root = tree.root_node();
        if root.kind() == "document" {
            return std::iter::once(root.byte_range()).collect();
        }
        let mut documents = Vec::new();
        let mut walker = root.walk();
        for child in root.children(&mut walker) {
            if child.kind() == "document" {
                documents.push(child.byte_range());
            }
        }
        if documents.is_empty() {
            return marker_split(rope);
        }
        documents
    }

    pub fn document_index_at(&self, rope: &Rope, offset: usize) -> usize {
        let documents = self.documents(rope);
        documents
            .iter()
            .position(|document| offset < document.end)
            .unwrap_or(documents.len().saturating_sub(1))
    }

    pub fn error_ranges(&self) -> Vec<Range<usize>> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut errors = Vec::new();
        collect_errors(tree.root_node(), &mut errors);
        errors
    }

    /// Whether YAML structure in one document can resolve mapping keys to text
    /// different from the spelling this syntax layer sees. Apply pruning is a
    /// byte edit, deliberately not a YAML decode; an escaped/tagged key or an
    /// alias can otherwise disguise a field the client is required to remove.
    ///
    /// This is intentionally conservative. Rewriting an unusual key to a plain
    /// or simply quoted scalar is preferable to sending server-owned state under
    /// a false claim that it was pruned.
    pub fn has_ambiguous_yaml_structure(&self, rope: &Rope, document_index: usize) -> bool {
        if self.language != LanguageKind::Yaml {
            return false;
        }
        let Some(scope) = self.documents(rope).get(document_index).cloned() else {
            return true;
        };
        let nodes = self.document_nodes();
        let Some(root) = nodes
            .get(document_index)
            .copied()
            .or_else(|| (nodes.len() == 1).then(|| nodes[0]))
        else {
            return true;
        };
        // Every node of the document is visited, so each per-node cost is paid
        // a hundred thousand times on a large manifest. Three of them were
        // avoidable: `node.walk()` allocates a tree-sitter cursor per node,
        // `kind()` compares strings where the grammar has already assigned
        // numbers, and `slice_to_string` copied every mapping key out of the
        // rope only to reject it. Together they cost 25 ms on a 31,000-line
        // document, and this guard runs on ctrl-s, before a diff a person is
        // waiting for.
        let language = root.language();
        let alias = language.id_for_node_kind("alias", true);
        let block_pair = language.id_for_node_kind("block_mapping_pair", true);
        let flow_pair = language.id_for_node_kind("flow_pair", true);
        let key_field = language.field_id_for_name("key");
        // A stream of one document fills its own root, so every node is inside
        // it by construction and asking each one where it starts and ends is
        // two calls per node for an answer that cannot be no.
        let confined = scope.start > root.start_byte() || scope.end < root.end_byte();
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            let inside = !confined || {
                let range = node.byte_range();
                range.end > scope.start && range.start < scope.end
            };
            if inside {
                let kind = node.kind_id();
                if kind == alias {
                    return true;
                }
                if (kind == block_pair || kind == flow_pair)
                    && let Some(field) = key_field
                    && let Some(key) = node.child_by_field_id(field.get())
                    && key_may_be_ambiguous(rope, &key.byte_range())
                    && ambiguous_yaml_key(&rope.slice_to_string(key.byte_range()))
                {
                    return true;
                }
                // Only named children can be an alias or carry a key; the
                // grammar's punctuation cannot, and it is a third of the tree.
                if node.named_child_count() > 0 && cursor.goto_first_child() {
                    continue;
                }
            }
            // Sideways, then up, stopping at the document this call is about:
            // the node beside it belongs to the next one.
            loop {
                if cursor.node().id() == root.id() {
                    return false;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return false;
                }
            }
        }
    }

    /// True when a path resolves to a literal mapping in the parsed document.
    /// An alias is deliberately false even if the server would expand it to one:
    /// the apply pruner never resolves aliases.
    pub fn is_mapping_at(&self, rope: &Rope, document_index: usize, path: &[PathSeg]) -> bool {
        self.resolve_path(rope, document_index, path)
            .and_then(mapping_under)
            .is_some()
    }

    pub fn mapping_keys_at(
        &self,
        rope: &Rope,
        document_index: usize,
        path: &[PathSeg],
    ) -> Vec<String> {
        let Some(node) = self.resolve_path(rope, document_index, path) else {
            return Vec::new();
        };
        let mapping = mapping_under(node);
        let Some(mapping) = mapping else {
            return Vec::new();
        };
        let within = (path.is_empty() && self.document_nodes().len() == 1)
            .then(|| self.documents(rope).get(document_index).cloned())
            .flatten();
        let mut keys = Vec::new();
        let mut walker = mapping.walk();
        for pair in mapping.named_children(&mut walker) {
            if within
                .as_ref()
                .is_some_and(|range| !range.contains(&pair.byte_range().start))
            {
                continue;
            }
            if let Some(key) = pair.child_by_field_name("key") {
                keys.push(scalar_text(rope, key));
            }
        }
        keys
    }

    pub(crate) fn document_nodes(&self) -> Vec<Node<'_>> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let root = tree.root_node();
        if root.kind() == "document" {
            return Vec::from([root]);
        }
        let mut walker = root.walk();
        let documents: Vec<Node<'_>> = root
            .children(&mut walker)
            .filter(|child| child.kind() == "document")
            .collect();
        if documents.is_empty() && root.named_child_count() > 0 {
            // A broken enough parse recovers as a bare ERROR root with no
            // document wrapper; the root still carries the pairs. Callers
            // index by document, so hand back one node per marker-split
            // range rather than one node for the whole stream.
            return Vec::from([root]);
        }
        documents
    }

    pub fn scalar_at(
        &self,
        rope: &Rope,
        document_index: usize,
        path: &[PathSeg],
    ) -> Option<String> {
        let node = self.resolve_path(rope, document_index, path)?;
        if mapping_under(node).is_some() || sequence_under(node).is_some() {
            return None;
        }
        Some(scalar_text(rope, node))
    }

    /// The whole `key: value` pair a path names -- key through the end of its
    /// value -- and how many entries its mapping holds. A caller removing a
    /// field has to remove the key with it, and has to know when doing so would
    /// leave an empty mapping behind: in a manifest `annotations:` with nothing
    /// under it is not an empty map, it is a request to delete every
    /// annotation.
    ///
    /// [`Resolved::Repeated`] is neither found nor absent, and a caller that
    /// treats it as either is guessing which of two copies the API server will
    /// read.
    pub fn pair_at(&self, rope: &Rope, document_index: usize, path: &[PathSeg]) -> Resolved {
        let Some((PathSeg::Key(wanted), parent)) = path.split_last() else {
            return Resolved::Absent;
        };
        let (node, scope) = match self.descend(rope, document_index, parent, OnRepeat::Stop) {
            Descent::At(node, scope) => (node, scope),
            Descent::Repeated => return Resolved::Repeated,
            Descent::Absent => return Resolved::Absent,
        };
        let Some(mapping) = mapping_under(node) else {
            return Resolved::Absent;
        };
        let mut walker = mapping.walk();
        // A comment is an extra the grammar hangs on whichever node it follows,
        // and one below the sole entry of a mapping is a named child of the
        // mapping itself. Counted as an entry it told the apply pruner that a
        // mapping it was about to empty still had something in it, and
        // `annotations:` went out with no value -- which an apply reads as a
        // request to delete every annotation.
        let pairs: Vec<Node<'_>> = mapping
            .named_children(&mut walker)
            .filter(|pair| !pair.is_extra())
            .filter(|pair| {
                scope
                    .as_ref()
                    .is_none_or(|range| range.contains(&pair.byte_range().start))
            })
            .collect();
        let mut matching = pairs.iter().filter(|pair| {
            pair.child_by_field_name("key")
                .is_some_and(|key| scalar_text(rope, key) == *wanted)
        });
        let Some(found) = matching.next() else {
            return Resolved::Absent;
        };
        if matching.next().is_some() {
            return Resolved::Repeated;
        }
        Resolved::Pair(Pair {
            bytes: found.byte_range(),
            siblings: pairs.len(),
        })
    }

    fn resolve_path(
        &self,
        rope: &Rope,
        document_index: usize,
        path: &[PathSeg],
    ) -> Option<Node<'_>> {
        match self.descend(rope, document_index, path, OnRepeat::TakeFirst) {
            Descent::At(node, _) => Some(node),
            Descent::Repeated | Descent::Absent => None,
        }
    }

    // The node a path names, plus whatever is left of the document scope: a
    // caller that walks one hop further itself has to apply the same
    // confinement the loop would have applied.
    fn descend(
        &self,
        rope: &Rope,
        document_index: usize,
        path: &[PathSeg],
        on_repeat: OnRepeat,
    ) -> Descent<'_> {
        if self.tree.is_none() {
            return Descent::Absent;
        }
        let Some(mut node) = self.document_nodes().into_iter().nth(document_index) else {
            return Descent::Absent;
        };
        // On a bare-ERROR recovery every document shares one node, so the
        // first hop is confined to this document's marker-split range.
        let mut scope = if self.document_nodes().len() == 1 {
            self.documents(rope).get(document_index).cloned()
        } else {
            None
        };
        for segment in path {
            match segment {
                PathSeg::Key(wanted) => {
                    let Some(mapping) = mapping_under(node) else {
                        return Descent::Absent;
                    };
                    let within = scope.take();
                    let mut pair_walker = mapping.walk();
                    let mut matching = mapping
                        .named_children(&mut pair_walker)
                        .filter(|pair| {
                            within
                                .as_ref()
                                .is_none_or(|range| range.contains(&pair.byte_range().start))
                        })
                        .filter(|pair| {
                            pair.child_by_field_name("key")
                                .is_some_and(|key| scalar_text(rope, key) == *wanted)
                        });
                    let Some(first) = matching.next() else {
                        return Descent::Absent;
                    };
                    // Lazy on purpose: `TakeFirst` never advances the iterator
                    // past the match, so the scan a keystroke pays for is the
                    // one it paid for before duplicates were detected at all.
                    if on_repeat == OnRepeat::Stop && matching.next().is_some() {
                        return Descent::Repeated;
                    }
                    match first.child_by_field_name("value") {
                        Some(value) => node = value,
                        None => return Descent::Absent,
                    }
                }
                PathSeg::Index(wanted) => {
                    let Some(sequence) = sequence_under(node) else {
                        return Descent::Absent;
                    };
                    let mut item_walker = sequence.walk();
                    let found = sequence
                        .named_children(&mut item_walker)
                        .filter(|item| !item.is_extra())
                        .nth(*wanted);
                    let Some(mut item) = found else {
                        return Descent::Absent;
                    };
                    if item.kind() == "block_sequence_item" {
                        match item.named_child(0) {
                            Some(inner) => item = inner,
                            None => return Descent::Absent,
                        }
                    }
                    node = item;
                }
            }
        }
        Descent::At(node, scope)
    }

    pub fn context_at(&self, rope: &Rope, offset: usize) -> CursorContext {
        let document_index = self.document_index_at(rope, offset);
        let (prefix, position, value_key) = self.cursor_shape(rope, offset);
        let from_tree = self.tree_path(rope, offset, &prefix, position);
        // The text-only fallback, which is what answers while the parse is
        // broken. Whichever knows more about where the cursor is wins.
        let from_text = match self.language {
            LanguageKind::Yaml => indent_path(rope, offset, &prefix),
            LanguageKind::Json => {
                let mut path = json_path(rope, offset - prefix.len());
                if position == CursorPosition::Value
                    && let Some(key) = &value_key
                {
                    path.push(PathSeg::Key(key.clone()));
                }
                path
            }
            LanguageKind::Plain => Vec::new(),
        };
        let mut path = match from_tree {
            Some(tree_path) if tree_path.len() >= from_text.len() => tree_path,
            _ => from_text,
        };
        if position == CursorPosition::Value
            && let Some(key) = &value_key
            && !matches!(path.last(), Some(PathSeg::Key(last)) if last == key)
        {
            path.push(PathSeg::Key(key.clone()));
        }
        CursorContext {
            string_site: self.string_site(rope, offset),
            path,
            prefix,
            position,
            value_key,
            document_index,
            language: self.language,
        }
    }

    fn string_site(&self, rope: &Rope, offset: usize) -> StringSite {
        let Some(tree) = self.tree.as_ref() else {
            return StringSite::Outside;
        };
        let mut current = tree
            .root_node()
            .descendant_for_byte_range(offset.saturating_sub(1), offset);
        while let Some(node) = current {
            if matches!(node.kind(), "string" | "string_content") {
                let range = node.byte_range();
                if range.end > offset {
                    return StringSite::Inside;
                }
                if range.end < offset {
                    return StringSite::Outside;
                }
                // The node ends exactly at the cursor: content the grammar could
                // not terminate leaves the string open, and a `string` carrying
                // both of its quotes is one the cursor has passed.
                if node.kind() == "string_content" {
                    return StringSite::Inside;
                }
                let text = rope.slice_to_string(range);
                return if text.len() >= 2 && text.ends_with('"') {
                    StringSite::After
                } else {
                    StringSite::Inside
                };
            }
            current = node.parent();
        }
        StringSite::Outside
    }

    // What is being typed, and whether it is a key or a value. YAML reads the
    // line, because a half-typed line is not in the tree yet. JSON reads the
    // tree instead: its values are quoted strings that routinely contain the
    // characters a line heuristic splits on -- an action name like
    // `k10s_shell::EditorSave` would otherwise be cut at its own colon and
    // completed onto itself.
    fn cursor_shape(&self, rope: &Rope, offset: usize) -> (String, CursorPosition, Option<String>) {
        if self.language != LanguageKind::Json {
            let prefix = word_prefix(rope, offset);
            let (position, value_key) = line_position(rope, offset, &prefix);
            return (prefix, position, value_key);
        }
        // A string the user is still typing has no closing quote, so the
        // grammar hands back a bare `string_content` inside an ERROR rather
        // than a `string`. That node is the token either way.
        let token_start = match self.string_content_at(offset) {
            Some(node) => node.byte_range().start,
            None => offset - word_prefix(rope, offset).len(),
        };
        let prefix = rope.slice_to_string(token_start..offset);
        // Step back off the opening quote and any whitespace: the structural
        // character before the token decides key or value, and starting from
        // the quote is what keeps a colon *inside* a string out of it.
        let mut at = token_start;
        if at > 0 && rope.char_at(at - 1) == Some('"') {
            at -= 1;
        }
        at = skip_back_while(rope, at, |character| character.is_whitespace());
        if at == 0 || rope.char_at(at - 1) != Some(':') {
            return (prefix, CursorPosition::Key, None);
        }
        (prefix, CursorPosition::Value, quoted_before(rope, at - 1))
    }

    fn string_content_at(&self, offset: usize) -> Option<Node<'_>> {
        let tree = self.tree.as_ref()?;
        let node = tree
            .root_node()
            .descendant_for_byte_range(offset.saturating_sub(1), offset)?;
        (node.kind() == "string_content" && node.byte_range().start < offset).then_some(node)
    }

    fn tree_path(
        &self,
        rope: &Rope,
        offset: usize,
        prefix: &str,
        position: CursorPosition,
    ) -> Option<Vec<PathSeg>> {
        let tree = self.tree.as_ref()?;
        let root = tree.root_node();
        let probe = offset.saturating_sub(prefix.len());
        let node = root.descendant_for_byte_range(probe, offset)?;
        let mut path = Vec::new();
        let mut child = node;
        while let Some(parent) = child.parent() {
            match parent.kind() {
                "block_mapping_pair" | "flow_pair" | "pair" => {
                    let key = parent.child_by_field_name("key");
                    let value = parent.child_by_field_name("value");
                    let in_key = key.is_some_and(|key| {
                        key.byte_range().contains(&offset)
                            || (key.byte_range().end == offset && value.is_none())
                    });
                    let key_is_child = key.is_some_and(|key| key.id() == child.id());
                    if !in_key
                        && !key_is_child
                        && let Some(key) = key
                    {
                        path.push(PathSeg::Key(scalar_text(rope, key)));
                    }
                }
                "block_sequence" | "flow_sequence" | "array" => {
                    let mut walker = parent.walk();
                    let index = parent
                        .named_children(&mut walker)
                        .filter(|item| !item.is_extra())
                        .position(|item| item.id() == child.id())
                        .unwrap_or(0);
                    path.push(PathSeg::Index(index));
                }
                _ => {}
            }
            child = parent;
        }
        path.reverse();
        if position == CursorPosition::Key
            && matches!(path.last(), Some(PathSeg::Key(last)) if last == prefix.trim())
            && !prefix.is_empty()
        {
            path.pop();
        }
        Some(path)
    }
}

const MAX_BACK_SCAN: usize = 128 << 10;
const MAX_PATH_DEPTH: usize = 64;

// JSON's answer to YAML's indentation scan: while the parse is broken, the
// unclosed brackets behind the cursor still describe where it sits. Walking
// back over them recovers the path, and jumping over string literals is what
// keeps a brace inside a value from counting as structure. Array indices are
// emitted as zero deliberately -- schema lookup treats every element of an
// array alike, so the shape is what matters, not the ordinal.
fn json_path(rope: &Rope, token_start: usize) -> Vec<PathSeg> {
    let mut path = Vec::new();
    // Start on the opening quote of the token being typed, not after it:
    // quote-jumping only stays aligned if every quote the scan meets is a
    // closing one.
    let mut at = match token_start.checked_sub(1) {
        Some(before) if rope.char_at(before) == Some('"') => before,
        _ => token_start,
    };
    let floor = at.saturating_sub(MAX_BACK_SCAN);
    let mut closed = 0usize;
    while at > floor && path.len() < MAX_PATH_DEPTH {
        let previous = rope.prev_char_offset(at);
        let character = rope.char_at(previous);
        at = previous;
        match character {
            Some('"') => at = string_start_before(rope, previous),
            Some('}' | ']') => closed += 1,
            Some('{' | '[') if closed > 0 => closed -= 1,
            Some('{') => {
                if let Some(key) = key_introducing(rope, previous) {
                    path.push(PathSeg::Key(key));
                }
            }
            Some('[') => path.push(PathSeg::Index(0)),
            _ => {}
        }
    }
    path.reverse();
    path
}

// The offset of the opening quote of the string whose closing quote sits at
// `quote`, so the scan can step over the whole literal.
fn string_start_before(rope: &Rope, quote: usize) -> usize {
    let mut at = quote;
    while at > 0 {
        let previous = rope.prev_char_offset(at);
        if rope.char_at(previous) == Some('"')
            && !(previous > 0 && rope.char_at(rope.prev_char_offset(previous)) == Some('\\'))
        {
            return previous;
        }
        at = previous;
    }
    0
}

// The key whose colon introduces the object opening at `brace`, if any: an
// object can also be an array element or the document root.
fn key_introducing(rope: &Rope, brace: usize) -> Option<String> {
    let at = skip_back_while(rope, brace, |character| character.is_whitespace());
    if at == 0 || rope.char_at(at - 1) != Some(':') {
        return None;
    }
    quoted_before(rope, at - 1)
}

// The k10s config loader accepts JSONC, and a trailing comma is the part of
// that dialect the grammar rejects. It is only trailing when the next thing
// in the document closes the container -- matching on the comma alone would
// silence every genuine stray comma too.
pub(crate) fn is_trailing_comma(rope: &Rope, range: &Range<usize>) -> bool {
    let end = range.end.min(rope.len());
    if rope.slice_to_string(range.start..end).trim() != "," {
        return false;
    }
    let mut at = end;
    while at < rope.len() {
        match rope.char_at(at) {
            Some(character) if character.is_whitespace() => at = rope.next_char_offset(at),
            Some('}' | ']') => return true,
            _ => return false,
        }
    }
    false
}

fn skip_back_while(rope: &Rope, mut offset: usize, keep: impl Fn(char) -> bool) -> usize {
    while offset > 0 {
        let previous = rope.prev_char_offset(offset);
        match rope.char_at(previous) {
            Some(character) if keep(character) => offset = previous,
            _ => break,
        }
    }
    offset
}

// The contents of the quoted string that ends just before `offset`, which in
// JSON is the only thing a key can be.
fn quoted_before(rope: &Rope, offset: usize) -> Option<String> {
    let end = skip_back_while(rope, offset, |character| character.is_whitespace());
    if end == 0 || rope.char_at(end - 1) != Some('"') {
        return None;
    }
    let mut start = end - 1;
    while start > 0 {
        let previous = rope.prev_char_offset(start);
        if rope.char_at(previous) == Some('"') {
            return Some(rope.slice_to_string(start..end - 1));
        }
        start = previous;
    }
    None
}

// A YAML stream whose parse collapsed into one bare ERROR root still has its
// document markers in the text, and the editor must keep them: completion and
// validation in document three must not answer with document one's kind.
fn marker_split(rope: &Rope) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut line_start = 0usize;
    // A marker line is exactly three dashes and then nothing but spacing. Held
    // as two counters over one byte pass, because materializing a line per row
    // to ask that question is what made completion cost milliseconds.
    let mut dashes = 0usize;
    let mut bare = true;
    let mut offset = 0usize;
    let mut close = |line_start: usize, dashes: usize, bare: bool, start: &mut usize| {
        if dashes == 3 && bare && line_start > *start {
            ranges.push(*start..line_start);
            *start = line_start;
        }
    };
    for chunk in rope.chunks() {
        for byte in chunk.bytes() {
            if byte == b'\n' {
                close(line_start, dashes, bare, &mut start);
                line_start = offset + 1;
                dashes = 0;
                bare = true;
            } else if byte == b'-' && dashes < 3 && offset == line_start + dashes {
                dashes += 1;
            } else if !matches!(byte, b' ' | b'\t' | b'\r') {
                bare = false;
            }
            offset += 1;
        }
    }
    close(line_start, dashes, bare, &mut start);
    ranges.push(start..rope.len());
    ranges
}

// Whole lines above an offset, nearest first, materialized a window at a time.
// The YAML indent walk has to read the lines above the cursor and cannot
// afford the prefix of the document: at a quarter of a megabyte that copy
// alone was twice a frame.
struct BackLines<'a> {
    rope: &'a Rope,
    // The rope text in `start..scan`, which is what is left to look at.
    window: String,
    start: usize,
    scan: usize,
    // The last yielded line still occupies the window's tail; it is cut on the
    // next call so the borrow the caller holds stays valid.
    pending: Option<usize>,
    done: bool,
}

impl<'a> BackLines<'a> {
    fn new(rope: &'a Rope, offset: usize) -> BackLines<'a> {
        BackLines {
            rope,
            window: String::new(),
            start: offset,
            scan: offset,
            pending: None,
            done: false,
        }
    }

    // The first call answers with the partial line the cursor sits on; each
    // one after that with the whole line above.
    fn next_line(&mut self) -> Option<(usize, &str)> {
        if let Some(cut) = self.pending.take() {
            self.window.truncate(cut);
        }
        if self.done {
            return None;
        }
        loop {
            if let Some(index) = self.window.rfind('\n') {
                self.pending = Some(index);
                self.scan = self.start + index;
                return Some((self.start + index + 1, &self.window[index + 1..]));
            }
            if self.start == 0 {
                self.done = true;
                return Some((0, &self.window));
            }
            self.grow();
        }
    }

    fn grow(&mut self) {
        // A line longer than the window doubles it rather than looping.
        let span = SCAN_WINDOW.max(self.window.len());
        let start = self
            .rope
            .snap_to_char_boundary(self.start.saturating_sub(span));
        let mut fresh = self.rope.slice_to_string(start..self.start);
        fresh.push_str(&self.window);
        self.window = fresh;
        self.start = start;
    }
}

fn ts_point(point: crate::rope::Point) -> tree_sitter::Point {
    tree_sitter::Point {
        row: point.row,
        column: point.column,
    }
}

pub(crate) fn mapping_under(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "block_mapping" | "flow_mapping" | "object") {
        return Some(node);
    }
    {
        // An ERROR node (or the bare root of a broken parse) carries pairs
        // directly, with no mapping wrapper; the node itself is the mapping.
        let mut walker = node.walk();
        if node
            .named_children(&mut walker)
            .any(|child| matches!(child.kind(), "block_mapping_pair" | "flow_pair" | "pair"))
        {
            return Some(node);
        }
    }
    let mut walker = node.walk();
    let found = node
        .named_children(&mut walker)
        .find(|child| matches!(child.kind(), "block_mapping" | "flow_mapping" | "object"));
    found.or_else(|| {
        let mut deeper = node.walk();
        node.named_children(&mut deeper)
            .find_map(|child| {
                matches!(child.kind(), "block_node" | "flow_node" | "document")
                    .then(|| mapping_under(child))
            })
            .flatten()
    })
}

pub(crate) fn sequence_under(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "block_sequence" | "flow_sequence" | "array") {
        return Some(node);
    }
    let mut walker = node.walk();
    let found = node
        .named_children(&mut walker)
        .find(|child| matches!(child.kind(), "block_sequence" | "flow_sequence" | "array"));
    found.or_else(|| {
        let mut deeper = node.walk();
        node.named_children(&mut deeper)
            .find_map(|child| {
                matches!(child.kind(), "block_node" | "flow_node" | "document")
                    .then(|| sequence_under(child))
            })
            .flatten()
    })
}

// The scalar shape a value node claims, unified across grammars. YAML plain
// scalars refine through their typed children; JSON numbers split on their
// text because the grammar has one number kind.
pub(crate) fn scalar_class(rope: &Rope, node: Node<'_>) -> Option<ScalarClass> {
    fn classify(rope: &Rope, node: Node<'_>, depth: usize) -> Option<ScalarClass> {
        let class = match node.kind() {
            "string_scalar" | "double_quote_scalar" | "single_quote_scalar" | "block_scalar" => {
                Some(ScalarClass::Str)
            }
            "integer_scalar" => Some(ScalarClass::Int),
            "float_scalar" => Some(ScalarClass::Float),
            "boolean_scalar" | "true" | "false" => Some(ScalarClass::Bool),
            "null_scalar" | "null" => Some(ScalarClass::Null),
            "string" => Some(ScalarClass::Str),
            "number" => {
                let text = rope.slice_to_string(node.byte_range());
                // 1e3 is an integer value however it is spelled, so ask what
                // the number is rather than how it was written.
                if text.parse::<i64>().is_ok() {
                    Some(ScalarClass::Int)
                } else if text.contains(['.', 'e', 'E']) {
                    match text.parse::<f64>() {
                        Ok(value) if value.fract() == 0.0 && value.abs() < 1e18 => {
                            Some(ScalarClass::Int)
                        }
                        _ => Some(ScalarClass::Float),
                    }
                } else {
                    Some(ScalarClass::Int)
                }
            }
            _ => None,
        };
        if let Some(class) = class {
            return Some(class);
        }
        if depth > 6 {
            return None;
        }
        let mut walker = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut walker).collect();
        for child in children {
            if let Some(found) = classify(rope, child, depth + 1) {
                return Some(found);
            }
        }
        if node.kind() == "plain_scalar" {
            return Some(ScalarClass::Str);
        }
        None
    }
    classify(rope, node, 0)
}

pub fn scalar_text(rope: &Rope, node: Node<'_>) -> String {
    let text = rope.slice_to_string(node.byte_range());
    let trimmed = text.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        });
    unquoted.unwrap_or(trimmed).to_string()
}

// Whether a key is even worth copying out of the rope to judge. Every shape
// `ambiguous_yaml_key` rejects carries one of these bytes: a merge key is
// `<<`, a tag, anchor or alias opens with its sigil, an escape lives inside
// quotes, and a multi-line key holds a break. An ordinary key -- which is
// nearly every key in a manifest -- is ruled out without an allocation.
fn key_may_be_ambiguous(rope: &Rope, range: &Range<usize>) -> bool {
    let mut at = range.start;
    while at < range.end {
        let chunk = rope.chunk_bytes_from(at);
        if chunk.is_empty() {
            return false;
        }
        let take = chunk.len().min(range.end - at);
        if chunk[..take].iter().any(|byte| {
            matches!(
                byte,
                b'<' | b'!' | b'&' | b'*' | b'"' | b'\'' | b'\n' | b'\r'
            )
        }) {
            return true;
        }
        at += take;
    }
    false
}

fn ambiguous_yaml_key(raw: &str) -> bool {
    let key = raw.trim();
    if key == "<<" || key.starts_with(['!', '&', '*']) || key.contains(['\n', '\r']) {
        return true;
    }
    if let Some(inner) = key
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return inner.contains('\\');
    }
    if let Some(inner) = key
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return inner.contains("''");
    }
    false
}

fn collect_errors(node: Node<'_>, errors: &mut Vec<Range<usize>>) {
    if errors.len() >= MAX_ERROR_RANGES {
        return;
    }
    if node.is_error() || node.is_missing() {
        let range = node.byte_range();
        errors.push(if range.is_empty() {
            range.start..range.start + 1
        } else {
            range
        });
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        collect_errors(child, errors);
    }
}

fn flatten_spans(
    mut spans: Vec<(Range<usize>, usize, TokenKind)>,
) -> Vec<(Range<usize>, TokenKind)> {
    spans.sort_by(|a, b| {
        a.0.start
            .cmp(&b.0.start)
            .then(b.0.end.cmp(&a.0.end))
            .then(a.1.cmp(&b.1))
    });
    let mut output: Vec<(Range<usize>, TokenKind)> = Vec::with_capacity(spans.len());
    let mut open: Vec<(Range<usize>, TokenKind)> = Vec::new();
    for (range, _, kind) in spans {
        if let Some(last) = open.last_mut()
            && last.0 == range
        {
            // Identical ranges are one node captured by two patterns; the
            // grammars disagree on pattern order, so the semantic capture
            // wins over the generic one instead.
            if last.1 != TokenKind::Property {
                last.1 = kind;
            }
            continue;
        }
        while let Some(outer) = open.last() {
            if outer.0.end <= range.start {
                let done = open.pop().expect("the stack was just probed");
                if done.0.start < done.0.end {
                    output.push(done);
                }
            } else {
                break;
            }
        }
        if let Some(outer) = open.last_mut() {
            if outer.0.start < range.start {
                output.push((outer.0.start..range.start, outer.1));
            }
            outer.0.start = outer.0.end.min(range.end);
        }
        open.push((range, kind));
    }
    while let Some(done) = open.pop() {
        if done.0.start < done.0.end {
            output.push(done);
        }
    }
    output.sort_by_key(|(range, _)| range.start);
    output
}

fn word_prefix(rope: &Rope, offset: usize) -> String {
    let row = rope.byte_to_point(offset).row;
    let line_start = rope.line_start(row);
    let head = rope.slice_to_string(line_start..offset);
    let word_start = head
        .rfind(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '/'))
        })
        .map(|index| index + head[index..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    head[word_start..].to_string()
}

fn line_position(rope: &Rope, offset: usize, prefix: &str) -> (CursorPosition, Option<String>) {
    let row = rope.byte_to_point(offset).row;
    let line_start = rope.line_start(row);
    let head = rope.slice_to_string(line_start..offset.saturating_sub(prefix.len()));
    let content = head.trim_start_matches([' ', '-']);
    match content.rfind(':') {
        Some(colon) if content[colon..].starts_with(": ") || content.ends_with(':') => {
            let key = content[..colon]
                .trim()
                .trim_matches(['"', '\''])
                .to_string();
            (CursorPosition::Value, Some(key))
        }
        _ => (CursorPosition::Key, None),
    }
}

fn indent_path(rope: &Rope, offset: usize, prefix: &str) -> Vec<PathSeg> {
    let _ = prefix;
    let mut lines = BackLines::new(rope, offset);
    let cursor_indent;
    let cursor_is_item;
    {
        let (_, head) = lines.next_line().unwrap_or((0, ""));
        cursor_indent = head
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        cursor_is_item = head.trim_start().starts_with('-');
    }
    let mut path: Vec<PathSeg> = Vec::new();
    let mut target = cursor_indent;
    let mut counting: Option<(usize, usize)> = None;
    if cursor_is_item {
        counting = Some((cursor_indent, 0));
    }
    while let Some((_, line)) = lines.next_line() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        let after = strip_comment(line[indent..].trim_end());
        let is_item = after == "-" || after.starts_with("- ");
        let content = if is_item {
            after[1..].trim_start()
        } else {
            after
        };
        let content_indent = indent + (after.len() - content.len());
        if let Some((item_indent, count)) = counting.as_mut() {
            if indent > *item_indent {
                continue;
            }
            if is_item && indent == *item_indent {
                *count += 1;
                continue;
            }
            path.push(PathSeg::Index(*count));
            target = *item_indent;
            counting = None;
        }
        if indent >= target {
            continue;
        }
        if is_item {
            if content_indent < target
                && let Some(key) = key_opening_a_block(content)
            {
                path.push(PathSeg::Key(key));
            }
            counting = Some((indent, 0));
            continue;
        }
        if let Some(key) = key_opening_a_block(content) {
            path.push(PathSeg::Key(key));
            target = indent;
            // Nothing above can sit further left than the margin, so the walk
            // is finished; without this the scan reads to byte zero on every
            // keystroke no matter how shallow the cursor is.
            if target == 0 && counting.is_none() {
                break;
            }
        }
    }
    if let Some((_, count)) = counting {
        path.push(PathSeg::Index(count));
    }
    while matches!(path.last(), Some(PathSeg::Index(_))) && path.len() == 1 {
        path.pop();
    }
    path.reverse();
    path
}

fn strip_comment(text: &str) -> &str {
    match text.find(" #") {
        Some(index) => text[..index].trim_end(),
        None => text,
    }
}

fn key_opening_a_block(content: &str) -> Option<String> {
    let key = content.strip_suffix(':')?;
    (!key.is_empty()).then(|| key.trim().trim_matches(['"', '\'']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, EditGroup, SelectionIntent};

    const MANIFEST: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  labels:\n    app: web\nspec:\n  replicas: 3\n  template:\n    spec:\n      containers:\n        - name: web\n          image: nginx:1.27\n        - name: sidecar\n          image: envoy\n";

    fn parsed(text: &str) -> (Rope, Syntax) {
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        (rope, syntax)
    }

    fn keys(context: &CursorContext) -> Vec<String> {
        context
            .path
            .iter()
            .map(|segment| match segment {
                PathSeg::Key(key) => key.clone(),
                PathSeg::Index(index) => format!("[{index}]"),
            })
            .collect()
    }

    #[test]
    fn a_clean_manifest_parses_and_highlights_keys_as_properties() {
        let (rope, syntax) = parsed(MANIFEST);
        let spans = syntax.highlights(&rope, 0..rope.len());
        assert!(!spans.is_empty());
        let api_version = spans
            .iter()
            .find(|(range, _)| range.start == 0)
            .expect("the first key is highlighted");
        assert_eq!(api_version.1, TokenKind::Property);
        assert_eq!(api_version.0.end, "apiVersion".len());
        assert!(
            spans.iter().any(|(_, kind)| *kind == TokenKind::Number),
            "replicas: 3 yields a number token"
        );
    }

    #[test]
    fn highlight_spans_never_overlap_even_when_captures_nest() {
        let (rope, syntax) = parsed("script: |\n  line one\n  line two\nflag: true\n");
        let spans = syntax.highlights(&rope, 0..rope.len());
        for pair in spans.windows(2) {
            assert!(
                pair[0].0.end <= pair[1].0.start,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Boolean));
    }

    #[test]
    fn incremental_edits_keep_the_tree_in_step_with_the_rope() {
        let mut buffer = Buffer::new(MANIFEST);
        let mut syntax = Syntax::yaml();
        syntax.reparse(buffer.rope());
        let offset = MANIFEST.find("replicas: 3").expect("fixture") + "replicas: ".len();
        let splices = buffer.edit(
            vec![(offset..offset + 1, "12".to_string())],
            EditGroup::Other,
            SelectionIntent::Collapse,
        );
        syntax.edit(buffer.rope(), &splices);
        let spans = syntax.highlights(buffer.rope(), 0..buffer.rope().len());
        let number = spans
            .iter()
            .find(|(range, _)| range.start == offset)
            .expect("the replacement is still a number token");
        assert_eq!(number.1, TokenKind::Number);
        assert_eq!(number.0.end - number.0.start, 2);
    }

    #[test]
    fn multi_document_streams_split_on_the_marker() {
        let (rope, syntax) = parsed("a: 1\n---\nb: 2\n---\nc: 3\n");
        assert_eq!(syntax.document_ranges(&rope).len(), 3);
        assert_eq!(syntax.document_index_at(&rope, 1), 0);
        assert_eq!(syntax.document_index_at(&rope, 10), 1);
        assert_eq!(syntax.document_index_at(&rope, rope.len() - 1), 2);
    }

    #[test]
    fn the_cursor_path_descends_mappings_and_sequence_items() {
        let (rope, syntax) = parsed(MANIFEST);
        let offset = MANIFEST.find("image: nginx").expect("fixture");
        let context = syntax.context_at(&rope, offset);
        assert_eq!(
            keys(&context),
            ["spec", "template", "spec", "containers", "[0]"]
        );
        assert_eq!(context.position, CursorPosition::Key);
        let second = MANIFEST.find("image: envoy").expect("fixture");
        let context = syntax.context_at(&rope, second);
        assert_eq!(
            keys(&context),
            ["spec", "template", "spec", "containers", "[1]"]
        );
    }

    #[test]
    fn a_value_cursor_names_its_key_and_prefix() {
        let text = "spec:\n  imagePullPolicy: Alw";
        let (rope, syntax) = parsed(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Value);
        assert_eq!(context.value_key.as_deref(), Some("imagePullPolicy"));
        assert_eq!(context.prefix, "Alw");
        assert_eq!(
            keys(&context),
            ["spec", "imagePullPolicy"],
            "a value path ends with its key so schema resolution is one lookup"
        );
    }

    #[test]
    fn a_half_typed_key_resolves_to_its_parent_mapping() {
        let text = "spec:\n  replicas: 3\n  temp";
        let (rope, syntax) = parsed(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Key);
        assert_eq!(context.prefix, "temp");
        assert_eq!(keys(&context), ["spec"]);
    }

    #[test]
    fn an_empty_line_between_keys_still_finds_the_mapping_by_indent() {
        let text = "spec:\n  template:\n    spec:\n      ";
        let (rope, syntax) = parsed(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(keys(&context), ["spec", "template", "spec"]);
        assert_eq!(context.position, CursorPosition::Key);
        assert_eq!(context.prefix, "");
    }

    #[test]
    fn mapping_keys_at_lists_the_siblings_for_completion_filtering() {
        let (rope, syntax) = parsed(MANIFEST);
        let top = syntax.mapping_keys_at(&rope, 0, &[]);
        assert_eq!(top, ["apiVersion", "kind", "metadata", "spec"]);
        let spec = syntax.mapping_keys_at(&rope, 0, &[PathSeg::Key("spec".into())]);
        assert_eq!(spec, ["replicas", "template"]);
        let container = syntax.mapping_keys_at(
            &rope,
            0,
            &[
                PathSeg::Key("spec".into()),
                PathSeg::Key("template".into()),
                PathSeg::Key("spec".into()),
                PathSeg::Key("containers".into()),
                PathSeg::Index(1),
            ],
        );
        assert_eq!(container, ["name", "image"]);
    }

    #[test]
    fn parse_errors_surface_as_bounded_ranges() {
        let (_, syntax) = parsed("a: [1, 2\nb: }\n");
        let errors = syntax.error_ranges();
        assert!(!errors.is_empty());
        assert!(errors.len() <= MAX_ERROR_RANGES);
        for range in &errors {
            assert!(range.start < range.end, "every error range is visible");
        }
    }

    const SETTINGS_JSON: &str = "{\n  // the workspace theme\n  \"theme\": \"one-dark\",\n  \"left_dock_width\": 260,\n  \"panels\": [\"files\", \"kinds\"]\n}\n";

    fn parsed_json(text: &str) -> (Rope, Syntax) {
        let rope = Rope::from(text);
        let mut syntax = Syntax::json();
        syntax.reparse(&rope);
        (rope, syntax)
    }

    #[test]
    fn json_parses_with_comments_and_highlights_keys_as_properties() {
        let (rope, syntax) = parsed_json(SETTINGS_JSON);
        let spans = syntax.highlights(&rope, 0..rope.len());
        let theme_key = SETTINGS_JSON.find("\"theme\"").expect("fixture");
        assert!(
            spans
                .iter()
                .any(|(range, kind)| range.start == theme_key && *kind == TokenKind::Property),
            "a pair key is a property: {spans:?}"
        );
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Comment));
        assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Number));
    }

    #[test]
    fn a_json_root_is_its_own_single_document() {
        let (rope, syntax) = parsed_json(SETTINGS_JSON);
        assert_eq!(syntax.document_ranges(&rope).len(), 1);
        assert_eq!(
            syntax.scalar_at(&rope, 0, &[PathSeg::Key("theme".into())]),
            Some("one-dark".to_string())
        );
        assert_eq!(
            syntax.scalar_at(
                &rope,
                0,
                &[PathSeg::Key("panels".into()), PathSeg::Index(1)]
            ),
            Some("kinds".to_string())
        );
    }

    #[test]
    fn a_json_cursor_derives_key_and_value_contexts() {
        let text = "{\n  \"theme\": \"one";
        let (rope, syntax) = parsed_json(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Value);
        assert_eq!(context.value_key.as_deref(), Some("theme"));
        assert_eq!(context.prefix, "one");
        assert_eq!(keys(&context), ["theme"]);

        let text = "{\n  \"theme\": \"one-dark\",\n  \"le";
        let (rope, syntax) = parsed_json(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Key);
        assert_eq!(context.prefix, "le");
        assert!(
            keys(&context).is_empty(),
            "a top-level key completes at the root: {:?}",
            context.path
        );
    }

    #[test]
    fn a_json_value_prefix_survives_colons_inside_the_string() {
        // The keymap file's action names contain "::" -- a line-based key
        // heuristic cuts the prefix at that colon and then completes the
        // action name onto its own tail.
        let text = "[\n  {\n    \"bindings\": {\n      \"ctrl-x\": \"k10s_shell::Edi";
        let (rope, syntax) = parsed_json(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Value);
        assert_eq!(
            context.prefix, "k10s_shell::Edi",
            "the whole string content is the prefix, colons and all"
        );
        assert_eq!(context.value_key.as_deref(), Some("ctrl-x"));
        assert_eq!(
            keys(&context),
            ["[0]", "bindings", "ctrl-x"],
            "the path reaches the bindings map even though the parse is broken"
        );
    }

    #[test]
    fn a_json_key_position_is_not_confused_by_a_previous_value() {
        let text = "{\n  \"url\": \"http://example.com:8080/x\",\n  \"th";
        let (rope, syntax) = parsed_json(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(
            context.position,
            CursorPosition::Key,
            "a colon inside the previous value does not make this a value"
        );
        assert_eq!(context.prefix, "th");
        assert_eq!(context.value_key, None);
    }

    #[test]
    fn an_empty_json_value_completes_with_no_prefix() {
        let text = "{\n  \"theme\": \"";
        let (rope, syntax) = parsed_json(text);
        let context = syntax.context_at(&rope, text.len());
        assert_eq!(context.position, CursorPosition::Value);
        assert_eq!(context.prefix, "");
        assert_eq!(context.value_key.as_deref(), Some("theme"));
    }

    #[test]
    fn json_mapping_keys_list_for_completion_filtering() {
        let (rope, syntax) = parsed_json(SETTINGS_JSON);
        assert_eq!(
            syntax.mapping_keys_at(&rope, 0, &[]),
            ["theme", "left_dock_width", "panels"]
        );
    }

    #[test]
    fn plain_text_answers_everything_with_graceful_empties() {
        let rope = Rope::from("just some notes\nwith lines\n");
        let mut syntax = Syntax::new(LanguageKind::Plain);
        syntax.reparse(&rope);
        assert!(!syntax.is_parsed());
        assert!(syntax.highlights(&rope, 0..rope.len()).is_empty());
        assert!(syntax.error_ranges().is_empty());
        assert_eq!(syntax.document_ranges(&rope).len(), 1);
    }

    #[test]
    fn language_kind_follows_the_file_extension() {
        assert_eq!(LanguageKind::from_file_name("web.yaml"), LanguageKind::Yaml);
        assert_eq!(LanguageKind::from_file_name("WEB.YML"), LanguageKind::Yaml);
        assert_eq!(
            LanguageKind::from_file_name("settings.json"),
            LanguageKind::Json
        );
        assert_eq!(
            LanguageKind::from_file_name("notes.txt"),
            LanguageKind::Plain
        );
        assert_eq!(
            LanguageKind::from_file_name("Makefile"),
            LanguageKind::Plain
        );
    }

    #[test]
    fn undo_shaped_full_reparse_recovers_from_any_tree_state() {
        let mut buffer = Buffer::new(MANIFEST);
        let mut syntax = Syntax::yaml();
        syntax.reparse(buffer.rope());
        buffer.edit(
            vec![(0..10, "x".to_string())],
            EditGroup::Other,
            SelectionIntent::Collapse,
        );
        buffer.undo();
        syntax.reparse(buffer.rope());
        let context =
            syntax.context_at(buffer.rope(), MANIFEST.find("name: web").expect("fixture"));
        assert_eq!(keys(&context), ["metadata"]);
    }
}
