//! Chunking: one item per definition as identified by the grammar's tags query; files without a
//! grammar (or without any definitions) are chunked into prose paragraphs. Component files
//! (`.svelte`) get both: their scripts' definitions and their markup as text.

#[cfg(feature = "lang-clarity")]
mod clarity;
mod component;
pub mod languages;

use std::cell::RefCell;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tree_sitter::{Parser, QueryCursor, StreamingIterator};
use tree_sitter_tags::TagsContext;

use component::Component;
pub use languages::{Language, LanguageInfo, LanguageRegistry, extension_of};

/// Kind assigned to prose chunks.
pub const TEXT_KIND: &str = "text";
/// Language assigned to prose chunks from files without a grammar.
pub const TEXT_LANGUAGE: &str = "text";
/// Bumped when items gain fields, so existing indexes re-chunk on the next full index.
const ITEM_FORMAT: u32 = 3;

/// One searchable definition (or prose chunk) extracted from a file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub rel_path: String,
    pub language: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    /// 1-based, inclusive.
    pub line_start: u32,
    pub line_end: u32,
    /// 1-based position of the name: its line, and its column counted in characters.
    #[serde(default)]
    pub name_line: u32,
    #[serde(default)]
    pub name_column: u32,
    pub signature: String,
    pub doc: String,
    pub is_test: bool,
    /// Whether this came from a grammar's tags query (vs prose fallback).
    pub tagged: bool,
    /// Text that is embedded and lexically indexed.
    pub text: String,
}

impl Item {
    /// Text of the item's doc vector: header plus doc comment; `None` when undocumented.
    pub fn doc_text(&self) -> Option<String> {
        let doc = self.doc.trim();
        (!doc.is_empty()).then(|| format!("{} {}\n{doc}", self.kind, self.qualified_name))
    }

    pub fn doc_first_line(&self) -> &str {
        self.doc
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
    }
}

thread_local! {
    static TAGS_CONTEXT: RefCell<TagsContext> = RefCell::new(TagsContext::new());
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

#[derive(Clone)]
pub struct Chunker {
    registry: Arc<LanguageRegistry>,
    /// Approximate chunk budget in characters (~4 chars per token).
    max_chars: usize,
    /// Also emit one `module` item per tagged file summarising its top-level definitions.
    module_chunks: bool,
    /// Embed only header + doc + signature (experiment; see docs/decision-log.md).
    summary_only: bool,
}

struct Def {
    kind: String,
    name: String,
    /// Byte offset of the name.
    name_start: usize,
    range: Range<usize>,
    doc: String,
    parent: Option<usize>,
    children: Vec<usize>,
}

impl Chunker {
    pub fn new(registry: Arc<LanguageRegistry>, max_tokens: usize) -> Self {
        Self {
            registry,
            max_chars: max_tokens.max(16) * 4,
            module_chunks: false,
            summary_only: false,
        }
    }

    pub fn with_summary_only(mut self, on: bool) -> Self {
        self.summary_only = on;
        self
    }

    pub fn with_module_chunks(mut self, on: bool) -> Self {
        self.module_chunks = on;
        self
    }

    /// Changes whenever chunk output for unchanged files of every language could change. What
    /// changes one extension's output is in [`LanguageRegistry::extension_fingerprints`].
    pub fn fingerprint(&self) -> String {
        let key = format!(
            "{ITEM_FORMAT}|{}|{}|{}",
            self.max_chars, self.module_chunks, self.summary_only
        );
        blake3::hash(key.as_bytes()).to_hex()[..16].to_string()
    }

    pub fn registry(&self) -> &Arc<LanguageRegistry> {
        &self.registry
    }

    /// Chunks one file. Returns no items for files the registry does not accept.
    pub fn chunk(&self, rel_path: &str, source: &[u8]) -> Vec<Item> {
        let path = Path::new(rel_path);
        if let Some(c) = self.registry.component_for(path) {
            return self.chunk_component(rel_path, c, source);
        }
        if let Some(lang) = self.registry.for_file(path, source) {
            let items = self.chunk_tagged(rel_path, &lang.name, lang, source, source);
            if !items.is_empty() {
                return items;
            }
            return self.chunk_text(rel_path, &lang.name, source);
        }
        if self.registry.is_text_extension(path) {
            return self.chunk_text(rel_path, TEXT_LANGUAGE, source);
        }
        Vec::new()
    }

    /// A component's script definitions (or, without any, its script as text) and its markup as
    /// text, all in the component's language.
    fn chunk_component(&self, rel_path: &str, c: &Component, source: &[u8]) -> Vec<Item> {
        let Some(parts) = PARSER.with(|p| c.split(&mut p.borrow_mut(), source)) else {
            return self.chunk_text(rel_path, c.name, source);
        };
        let mut items = self
            .registry
            .by_name(parts.script_language())
            .map(|lang| self.chunk_tagged(rel_path, c.name, lang, &parts.script, source))
            .unwrap_or_default();
        if items.is_empty() {
            items = self.chunk_text(rel_path, c.name, &parts.script);
        }
        items.extend(self.chunk_text(rel_path, c.name, &parts.markup));
        items
    }

    /// Items for the definitions `lang`'s tags query finds in `source`, labelled `language`.
    /// `original` is the file as read, of which `source` may be a copy with parts blanked.
    fn chunk_tagged(
        &self,
        rel_path: &str,
        language: &str,
        lang: &Language,
        source: &[u8],
        original: &[u8],
    ) -> Vec<Item> {
        let mut defs: Vec<Def> = TAGS_CONTEXT.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            let Ok((tags, _)) = ctx.generate_tags(&lang.tags, source, None) else {
                return Vec::new();
            };
            tags.filter_map(Result::ok)
                .filter(|t| t.is_definition)
                .filter_map(|t| {
                    let name = std::str::from_utf8(&source[t.name_range.clone()])
                        .ok()?
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    if name.is_empty() {
                        return None;
                    }
                    Some(Def {
                        kind: lang.tags.syntax_type_name(t.syntax_type_id).to_string(),
                        name,
                        name_start: t.name_range.start,
                        range: t.range.clone(),
                        doc: t.docs.unwrap_or_default().trim().to_string(),
                        parent: None,
                        children: Vec::new(),
                    })
                })
                .collect()
        });
        if defs.is_empty() {
            return Vec::new();
        }
        defs.sort_by_key(|d| (d.range.start, std::cmp::Reverse(d.range.end)));
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..defs.len() {
            while let Some(&top) = stack.last() {
                if defs[top].range.end >= defs[i].range.end
                    && defs[top].range.start <= defs[i].range.start
                {
                    break;
                }
                stack.pop();
            }
            if let Some(&top) = stack.last() {
                defs[i].parent = Some(top);
                defs[top].children.push(i);
            }
            stack.push(i);
        }

        let tests = test_ranges(lang, source);
        let lines = LineIndex::new(source);
        let text = String::from_utf8_lossy(source);
        let mut qualified: Vec<String> = Vec::with_capacity(defs.len());
        let mut signatures: Vec<String> = Vec::with_capacity(defs.len());
        for d in &defs {
            let q = match d.parent {
                Some(p) => format!("{}::{}", qualified[p], d.name),
                None => d.name.clone(),
            };
            qualified.push(q);
            signatures.push(signature(slice(&text, d.range.clone())));
        }
        let module = self
            .module_chunks
            .then(|| {
                module_item(
                    rel_path,
                    language,
                    &text,
                    &defs,
                    &signatures,
                    &lines,
                    self.max_chars,
                )
            })
            .flatten();
        let mut items: Vec<Item> = defs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let own = own_source(&text, d, &defs, &signatures);
                let mut body = format!("{rel_path} {} {}\n", d.kind, qualified[i]);
                if !d.doc.is_empty() {
                    body.push_str(&d.doc);
                    body.push('\n');
                }
                if self.summary_only {
                    body.push_str(&signatures[i]);
                } else {
                    body.push_str(&own);
                }
                let (name_line, name_column) = lines.position(original, d.name_start);
                Item {
                    rel_path: rel_path.to_string(),
                    language: language.to_string(),
                    kind: d.kind.clone(),
                    name: d.name.clone(),
                    qualified_name: qualified[i].clone(),
                    line_start: lines.line_of(d.range.start),
                    line_end: lines.line_of(d.range.end.saturating_sub(1).max(d.range.start)),
                    name_line,
                    name_column,
                    signature: signatures[i].clone(),
                    doc: d.doc.clone(),
                    is_test: tests
                        .iter()
                        .any(|r| r.start <= d.range.start && r.end >= d.range.end),
                    tagged: true,
                    text: truncate(&body, self.max_chars),
                }
            })
            .collect();
        items.extend(module);
        items
    }

    /// Blank-line-separated paragraphs merged up to the chunk budget; a paragraph over the
    /// budget is cut at line boundaries first.
    fn chunk_text(&self, rel_path: &str, language: &str, source: &[u8]) -> Vec<Item> {
        let text = String::from_utf8_lossy(source);
        // (first line, lines) of each paragraph or paragraph piece.
        let mut pieces: Vec<(u32, Vec<&str>)> = Vec::new();
        let mut cur: Option<(u32, Vec<&str>, usize)> = None;
        for (i, line) in text.lines().enumerate() {
            let lineno = i as u32 + 1;
            if line.trim().is_empty() {
                if let Some((first, lines, _)) = cur.take() {
                    pieces.push((first, lines));
                }
                continue;
            }
            let (first, lines, len) = cur.get_or_insert((lineno, Vec::new(), 0));
            if *len > 0 && *len + line.len() + 1 > self.max_chars {
                pieces.push((*first, std::mem::take(lines)));
                (*first, *len) = (lineno, 0);
            }
            lines.push(line);
            *len += line.len() + 1;
        }
        if let Some((first, lines, _)) = cur {
            pieces.push((first, lines));
        }
        let mut items = Vec::new();
        let mut buf = String::new();
        let (mut start, mut end) = (0u32, 0u32);
        for (first, lines) in pieces {
            let para = lines.join("\n");
            if !buf.is_empty() && buf.len() + para.len() + 2 > self.max_chars {
                items.push(self.text_item(rel_path, language, &buf, start, end));
                buf.clear();
            }
            if buf.is_empty() {
                start = first;
            } else {
                buf.push_str("\n\n");
            }
            buf.push_str(&para);
            end = first + lines.len() as u32 - 1;
        }
        if !buf.is_empty() {
            items.push(self.text_item(rel_path, language, &buf, start, end));
        }
        items
    }

    /// A prose item for `text`, lines `start..=end`, named after its first line.
    fn text_item(&self, rel_path: &str, language: &str, text: &str, start: u32, end: u32) -> Item {
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let indent = first.chars().take_while(|c| c.is_whitespace()).count() as u32;
        let name: String = first.trim().chars().take(80).collect();
        Item {
            rel_path: rel_path.to_string(),
            language: language.to_string(),
            kind: TEXT_KIND.to_string(),
            name: name.clone(),
            qualified_name: name.clone(),
            line_start: start,
            line_end: end.max(start),
            name_line: start,
            name_column: indent + 1,
            signature: name,
            doc: String::new(),
            is_test: false,
            tagged: false,
            text: truncate(
                &format!("{rel_path} {TEXT_KIND}\n{}", text.trim()),
                self.max_chars,
            ),
        }
    }
}

/// A file-level item: the text before the first definition (typically module docs) plus the
/// signatures of top-level definitions. Only for files with at least two top-level definitions.
fn module_item(
    rel_path: &str,
    language: &str,
    text: &str,
    defs: &[Def],
    signatures: &[String],
    lines: &LineIndex,
    max_chars: usize,
) -> Option<Item> {
    let top: Vec<usize> = (0..defs.len())
        .filter(|&i| defs[i].parent.is_none())
        .collect();
    if top.len() < 2 {
        return None;
    }
    let lead = slice(text, 0..defs[top[0]].range.start).trim().to_string();
    let name = Path::new(rel_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut body = format!("{rel_path} module {name}\n{lead}\n");
    for &i in &top {
        body.push_str(&signatures[i]);
        body.push('\n');
    }
    Some(Item {
        rel_path: rel_path.to_string(),
        language: language.to_string(),
        kind: "module".to_string(),
        name: name.clone(),
        qualified_name: rel_path.to_string(),
        line_start: 1,
        line_end: lines.line_of(text.len().saturating_sub(1)),
        name_line: 1,
        name_column: 1,
        signature: format!("module {rel_path}"),
        doc: String::new(),
        is_test: false,
        tagged: true,
        text: truncate(&body, max_chars),
    })
}

/// Byte ranges of nodes captured by the language's tests query.
fn test_ranges(lang: &Language, source: &[u8]) -> Vec<Range<usize>> {
    let Some(query) = &lang.tests else {
        return Vec::new();
    };
    let Some(test_idx) = query.capture_index_for_name("test") else {
        return Vec::new();
    };
    PARSER.with(|p| {
        let mut parser = p.borrow_mut();
        if parser.set_language(&lang.grammar).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut matches = cursor.matches(query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for c in m.captures().iter().filter(|c| c.index == test_idx) {
                out.push(c.node.byte_range());
            }
        }
        out
    })
}

/// Definition source with each direct child definition collapsed to its signature, so a
/// container's chunk is its own text plus member signatures.
fn own_source(text: &str, d: &Def, defs: &[Def], signatures: &[String]) -> String {
    let mut out = String::new();
    let mut pos = d.range.start;
    for &c in &d.children {
        let child = &defs[c];
        if child.range.start < pos {
            continue;
        }
        out.push_str(slice(text, pos..child.range.start));
        out.push_str(&signatures[c]);
        out.push_str(" …");
        pos = child.range.end;
    }
    out.push_str(slice(text, pos..d.range.end));
    out
}

fn slice(text: &str, r: Range<usize>) -> &str {
    let mut start = r.start.min(text.len());
    let mut end = r.end.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    &text[start..end]
}

/// One-line signature: the definition's text up to its body opener — `{` or `;` at bracket
/// depth 0, or a line ending in `:` — with whitespace collapsed. Falls back to the first line
/// when a blank line comes first, or when the definition closes with an `end` line (Ruby, Lua),
/// whose header has no opener.
pub fn signature(def_text: &str) -> String {
    let first_line = def_text.find('\n').unwrap_or(def_text.len());
    let closes_with_end = def_text.trim_end().rsplit('\n').next().map(str::trim) == Some("end");
    let end = if closes_with_end {
        first_line
    } else {
        header_end(def_text).unwrap_or(first_line)
    };
    let mut s = slice(def_text, 0..end)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if s.chars().count() > 240 {
        s = s.chars().take(240).collect();
    }
    s
}

/// Byte offset of the body opener [`signature`] looks for, the whole text when it has none, or
/// `None` to fall back to the first line.
fn header_end(def_text: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let bytes = def_text.as_bytes();
    let mut prev = b' ';
    for (i, &b) in bytes.iter().enumerate().take(600) {
        match b {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'<' => depth += 1,
            b'>' if prev != b'-' && prev != b'=' => depth -= 1,
            b'{' | b';' if depth <= 0 => return Some(i),
            b':' if depth <= 0 => {
                let rest = &def_text[i + 1..];
                if rest.starts_with('\n') || rest.starts_with("\r\n") || rest.is_empty() {
                    return Some(i);
                }
            }
            b'\n' => {
                let rest = &def_text[i + 1..];
                if rest.trim_start_matches([' ', '\t', '\r']).starts_with('\n') {
                    return None;
                }
            }
            _ => {}
        }
        prev = b;
    }
    (bytes.len() < 600).then_some(def_text.len())
}

/// Truncates to at most `max` bytes on a char boundary.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    /// 1-based line number containing byte offset `pos`.
    fn line_of(&self, pos: usize) -> u32 {
        match self.starts.binary_search(&pos) {
            Ok(i) => i as u32 + 1,
            Err(i) => i as u32,
        }
    }

    /// 1-based (line, column) of byte offset `pos`, the column counted in characters.
    fn position(&self, source: &[u8], pos: usize) -> (u32, u32) {
        let line = self.line_of(pos);
        let start = self.starts[line as usize - 1];
        let before = &source[start..pos.min(source.len())];
        let column = String::from_utf8_lossy(before).chars().count() as u32 + 1;
        (line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguagesConfig;

    fn chunker() -> Chunker {
        let cfg = LanguagesConfig {
            tree_sitter_config: None,
            ..LanguagesConfig::default()
        };
        let reg = LanguageRegistry::load(&cfg, Path::new("/nonexistent"));
        assert!(reg.issues.is_empty(), "{:?}", reg.issues);
        Chunker::new(Arc::new(reg), 300)
    }

    #[test]
    fn rust_definitions_with_docs_and_nesting() {
        let src = br#"
/// Wrap a Read so that we store a copy of what was read.
#[derive(Debug)]
pub struct RetryReader<'a, R: Read> {
    fd: &'a mut R,
}

impl<'a, R: Read> RetryReader<'a, R> {
    /// Create a new reader.
    pub fn new(fd: &'a mut R) -> Self {
        Self { fd }
    }
}

/// Declare a guarded string type.
#[macro_export]
macro_rules! guarded_string {
    ($name:ident) => {};
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}

#[test]
/// Doc after the attribute.
fn documented_test() {}

#[rstest]
#[case(1)]
fn param_test(#[case] x: u8) {}

#[tokio::test]
async fn async_test() {}
"#;
        let items = chunker().chunk("src/retry.rs", src);
        let find = |n: &str| {
            items
                .iter()
                .find(|i| i.name == n)
                .unwrap_or_else(|| panic!("{n} missing: {items:#?}"))
        };
        let rr = find("RetryReader");
        assert_eq!(rr.kind, "class");
        assert_eq!(
            rr.doc,
            "Wrap a Read so that we store a copy of what was read."
        );
        assert_eq!(rr.signature, "pub struct RetryReader<'a, R: Read>");
        assert_eq!(rr.line_start, 4);
        let new = items.iter().find(|i| i.name == "new").unwrap();
        assert_eq!(new.qualified_name, "RetryReader::new");
        assert_eq!(new.kind, "method");
        assert_eq!(new.doc, "Create a new reader.");
        let m = find("guarded_string");
        assert_eq!(m.kind, "macro");
        assert_eq!(m.doc, "Declare a guarded string type.");
        assert!(find("it_works").is_test);
        let dt = find("documented_test");
        assert!(dt.is_test);
        assert_eq!(dt.doc, "Doc after the attribute.");
        assert!(find("param_test").is_test);
        assert!(find("async_test").is_test);
        assert!(!rr.is_test);
    }

    #[test]
    fn python_docstrings_and_methods() {
        let src = b"class Foo:\n    \"\"\"A foo.\"\"\"\n\n    def bar(self):\n        \"\"\"Do bar.\"\"\"\n        return 1\n\ndef test_x():\n    pass\n";
        let items = chunker().chunk("pkg/foo.py", src);
        let foo = items.iter().find(|i| i.name == "Foo").unwrap();
        assert_eq!(foo.doc, "A foo.");
        let bar = items.iter().find(|i| i.name == "bar").unwrap();
        assert_eq!(bar.qualified_name, "Foo::bar");
        assert_eq!(bar.kind, "method");
        assert_eq!(bar.doc, "Do bar.");
        assert!(items.iter().find(|i| i.name == "test_x").unwrap().is_test);
    }

    /// The item named `name`, or a panic listing all items.
    fn item<'a>(items: &'a [Item], name: &str) -> &'a Item {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("{name} missing: {items:#?}"))
    }

    #[cfg(feature = "lang-java")]
    #[test]
    fn java_definitions_docs_and_tests() {
        let src = br#"/** License header. */

/**
 * A widget.
 * Second line.
 */
@Deprecated
public class Widget extends Base {
    /** Make one. */
    public Widget() {}

    // Runs it.
    @Override
    public void run() {}

    enum Mode { A, B }

    record Point(int x, int y) {}

    @interface Marker {}
}

class WidgetTest {
    @ParameterizedTest
    @ValueSource(ints = {1})
    void param(int x) {}
}
"#;
        let items = chunker().chunk("src/Widget.java", src);
        let w = item(&items, "Widget");
        assert_eq!(
            (w.kind.as_str(), w.doc.as_str()),
            ("class", "A widget.\nSecond line.")
        );
        assert_eq!(w.signature, "@Deprecated public class Widget extends Base");
        let ctor = items
            .iter()
            .find(|i| i.qualified_name == "Widget::Widget")
            .unwrap();
        assert_eq!(
            (ctor.kind.as_str(), ctor.doc.as_str()),
            ("method", "Make one.")
        );
        assert_eq!(item(&items, "run").doc, "Runs it.");
        assert_eq!(item(&items, "Mode").kind, "class");
        assert_eq!(item(&items, "Point").kind, "class");
        assert_eq!(item(&items, "Marker").kind, "interface");
        assert!(!w.is_test);
        assert!(item(&items, "WidgetTest").is_test);
        assert!(item(&items, "param").is_test);
    }

    #[cfg(feature = "lang-c")]
    #[test]
    fn c_definitions_span_bodies_with_docs() {
        let src = br#"/* Copyright header. */

#ifndef POINT_H
#define POINT_H
#define BUF_SIZE 64
#define MAX(a, b) ((a) > (b) ? (a) : (b))

/** A point. */
struct point {
    int x;
};

typedef enum { RED, GREEN } color_t;

/* Add two numbers. */
int add(int a, int b);

/**
 * Add two numbers.
 */
int add(int a, int b) {
    return a + b;
}

static char **names(void) { return 0; }

void test_add(void) {}
#endif
"#;
        let items = chunker().chunk("src/point.c", src);
        let p = item(&items, "point");
        assert_eq!((p.kind.as_str(), p.doc.as_str()), ("class", "A point."));
        assert_eq!(item(&items, "color_t").kind, "type");
        assert_eq!(item(&items, "MAX").kind, "macro");
        assert_eq!(item(&items, "BUF_SIZE").kind, "constant");
        assert!(items.iter().all(|i| i.name != "POINT_H"), "{items:#?}");
        let adds: Vec<_> = items.iter().filter(|i| i.name == "add").collect();
        assert_eq!(adds.len(), 2, "prototype and definition");
        let def = adds.iter().find(|i| i.line_end > i.line_start).unwrap();
        assert_eq!(def.doc, "Add two numbers.");
        assert_eq!(def.signature, "int add(int a, int b)");
        assert!(def.text.contains("return a + b;"));
        assert_eq!(item(&items, "names").signature, "static char **names(void)");
        assert!(item(&items, "test_add").is_test);
        assert!(!def.is_test);
    }

    #[cfg(feature = "lang-cpp")]
    #[test]
    fn cpp_members_templates_and_namespaces() {
        let src = br#"/// Geometry helpers.
namespace geo {

/// A shape.
class Shape {
public:
    /// Area.
    virtual double area() const = 0;
    int sides() const { return n_; }
    bool operator==(const Shape& o) const;
    ~Shape();
private:
    int n_;
};

/// Max of two.
template <typename T>
T max_of(T a, T b) { return a > b ? a : b; }

// Out of line.
double Shape::area() const { return 0; }

}  // namespace geo

TEST(ShapeTest, Sides) { EXPECT_EQ(1, 1); }
"#;
        let items = chunker().chunk("src/shape.cpp", src);
        let ns = item(&items, "geo");
        assert_eq!(
            (ns.kind.as_str(), ns.doc.as_str()),
            ("module", "Geometry helpers.")
        );
        let shape = item(&items, "Shape");
        assert_eq!(
            (shape.kind.as_str(), shape.doc.as_str()),
            ("class", "A shape.")
        );
        let area = items
            .iter()
            .find(|i| i.qualified_name == "geo::Shape::area")
            .unwrap();
        assert_eq!((area.kind.as_str(), area.doc.as_str()), ("method", "Area."));
        for m in ["sides", "operator==", "~Shape"] {
            assert_eq!(item(&items, m).qualified_name, format!("geo::Shape::{m}"));
        }
        let out_of_line = items
            .iter()
            .find(|i| i.qualified_name == "geo::area")
            .unwrap();
        assert_eq!(out_of_line.kind, "method");
        assert_eq!(out_of_line.doc, "Out of line.");
        let max = item(&items, "max_of");
        assert_eq!(
            (max.kind.as_str(), max.doc.as_str()),
            ("function", "Max of two.")
        );
        assert_eq!(max.signature, "template <typename T> T max_of(T a, T b)");
        assert!(item(&items, "TEST").is_test);
        assert!(!max.is_test);
    }

    #[cfg(all(feature = "lang-c", feature = "lang-cpp"))]
    #[test]
    fn headers_are_c_unless_they_use_cpp_syntax() {
        let c = b"/* Ring buffer. */\n#include <stddef.h>\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n/* Push a value; `class` in a comment is not C++. */\nint ring_push(struct ring *new, int v);\n\n#ifdef __cplusplus\n}\n#endif\n";
        let class = b"#pragma once\n\nclass Shape {\npublic:\n    double area() const;\n};\n";
        let std_only = b"#include <string>\n\nstd::string greet(const std::string& who);\n";
        let chunker = chunker();
        let languages = |src: &[u8]| -> Vec<String> {
            chunker
                .chunk("include/x.h", src)
                .into_iter()
                .map(|i| i.language)
                .collect()
        };
        assert!(languages(c).iter().all(|l| l == "c"), "{:?}", languages(c));
        assert_eq!(
            item(&chunker.chunk("include/ring.h", c), "ring_push").signature,
            "int ring_push(struct ring *new, int v)"
        );
        assert!(languages(class).iter().all(|l| l == "cpp"));
        let shape = chunker.chunk("include/shape.h", class);
        assert_eq!(item(&shape, "area").qualified_name, "Shape::area");
        assert!(languages(std_only).iter().all(|l| l == "cpp"));
        assert_eq!(
            item(&chunker.chunk("include/greet.h", std_only), "greet").signature,
            "std::string greet(const std::string& who)"
        );
    }

    #[cfg(all(feature = "lang-c", feature = "lang-cpp"))]
    #[test]
    fn mapped_headers_are_not_sniffed() {
        let mut cfg = LanguagesConfig {
            tree_sitter_config: None,
            ..LanguagesConfig::default()
        };
        cfg.extensions.insert("H".into(), "c".into());
        let reg = LanguageRegistry::load(&cfg, Path::new("/nonexistent"));
        let class = b"class Shape {};\n";
        assert_eq!(reg.for_file(Path::new("x.h"), class).unwrap().name, "c");
        let sniffing = chunker().registry().extension_fingerprints();
        assert_ne!(reg.extension_fingerprints()["h"], sniffing["h"]);
    }

    #[cfg(feature = "lang-php")]
    #[test]
    fn php_definitions_docs_and_tests() {
        let src = br#"<?php
/**
 * A widget.
 */
class Widget
{
    const MAX = 10;

    /** Make one. */
    public function __construct(int $n) {}
}

/** Mode values. */
enum Mode: string { case A = 'a'; }

class WidgetTest extends TestCase
{
    /** @test */
    public function it_runs(): void {}
}
"#;
        let items = chunker().chunk("src/Widget.php", src);
        let w = item(&items, "Widget");
        assert_eq!((w.kind.as_str(), w.doc.as_str()), ("class", "A widget."));
        let ctor = item(&items, "__construct");
        assert_eq!(
            (
                ctor.kind.as_str(),
                ctor.doc.as_str(),
                ctor.qualified_name.as_str()
            ),
            ("method", "Make one.", "Widget::__construct")
        );
        assert_eq!(item(&items, "MAX").kind, "constant");
        assert_eq!(item(&items, "Mode").doc, "Mode values.");
        assert!(item(&items, "it_runs").is_test);
        assert!(!w.is_test);
    }

    #[cfg(feature = "lang-lua")]
    #[test]
    fn lua_definitions_and_docs() {
        let src = b"--- Push a value.\n-- @param v the value\nfunction M.push(r, v)\n  r.head = r.head + 1\nend\n\n-- Size of the ring.\nfunction M:size()\n  return 0\nend\n\nfunction TestRing:test_push() end\n";
        let items = chunker().chunk("lua/ring.lua", src);
        let push = item(&items, "push");
        assert_eq!(push.doc, "Push a value.\n@param v the value");
        assert_eq!(push.signature, "function M.push(r, v)");
        let size = item(&items, "size");
        assert_eq!(
            (size.kind.as_str(), size.doc.as_str()),
            ("method", "Size of the ring.")
        );
        assert!(item(&items, "test_push").is_test);
        assert!(!push.is_test);
    }

    #[cfg(feature = "lang-elixir")]
    #[test]
    fn elixir_definitions_docs_and_tests() {
        let src = br#"defmodule MyApp.Widget do
  @moduledoc """
  Widgets.
  """

  @doc """
  Runs it.
  """
  @spec run(integer) :: integer
  def run(x) when is_integer(x) do
    x
  end

  defp helper(a), do: a
end

defimpl MyApp.Size, for: Map do
  def size(m), do: map_size(m)
end

defmodule MyApp.WidgetTest do
  use ExUnit.Case
  def setup_thing, do: :ok
end
"#;
        let items = chunker().chunk("lib/widget.ex", src);
        let modules: Vec<_> = items.iter().filter(|i| i.name == "MyApp.Widget").collect();
        assert_eq!(modules.len(), 1, "{items:#?}");
        assert_eq!(
            (modules[0].kind.as_str(), modules[0].doc.as_str()),
            ("module", "Widgets.")
        );
        let run = item(&items, "run");
        assert_eq!(run.doc, "Runs it.");
        assert_eq!(run.qualified_name, "MyApp.Widget::run");
        assert_eq!(run.signature, "def run(x) when is_integer(x) do");
        assert_eq!(item(&items, "helper").kind, "function");
        assert_eq!(item(&items, "MyApp.Size").kind, "impl");
        assert!(item(&items, "setup_thing").is_test);
        assert!(!run.is_test);
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn swift_definitions_docs_and_tests() {
        let src = br#"/// A widget.
public class Widget {
    /// The count.
    var count = 0

    /// Make one.
    init(n: Int) {}

    func run() -> Int {
        let local = 1
        return local
    }
}

extension Widget {
    func extra() {}
}

/// Shapes.
protocol Shape {
    func area() -> Double
}

/// Add two numbers.
func add(_ a: Int, _ b: Int) -> Int { a + b }

@Test func addsNumbers() {}
"#;
        let items = chunker().chunk("Sources/Widget.swift", src);
        let w = items
            .iter()
            .find(|i| i.name == "Widget" && i.kind == "class")
            .unwrap();
        assert_eq!(w.doc, "A widget.");
        let count = item(&items, "count");
        assert_eq!(
            (count.kind.as_str(), count.doc.as_str()),
            ("property", "The count.")
        );
        assert!(items.iter().all(|i| i.name != "local"), "{items:#?}");
        let init = item(&items, "init");
        assert_eq!(
            (init.kind.as_str(), init.doc.as_str()),
            ("method", "Make one.")
        );
        assert_eq!(item(&items, "run").qualified_name, "Widget::run");
        assert!(items.iter().any(|i| i.name == "Widget" && i.kind == "impl"));
        assert_eq!(item(&items, "area").qualified_name, "Shape::area");
        assert_eq!(item(&items, "Shape").kind, "interface");
        let add = item(&items, "add");
        assert_eq!(
            (add.kind.as_str(), add.doc.as_str()),
            ("function", "Add two numbers.")
        );
        assert!(item(&items, "addsNumbers").is_test);
        assert!(!add.is_test);
    }

    #[cfg(feature = "lang-csharp")]
    #[test]
    fn csharp_definitions_docs_and_tests() {
        let src = br#"namespace Acme.Widgets
{
    /// <summary>
    /// A widget.
    /// </summary>
    [Serializable]
    public class Widget
    {
        /// <summary>The count.</summary>
        public int Count { get; set; }

        public void Run() {}
    }

    public enum Mode { A, B }

    public interface IRunnable { void Run(); }

    public class Plain
    {
        [Theory]
        [InlineData(1)]
        public void Param(int x) {}

        public void Helper() {}
    }
}
"#;
        let items = chunker().chunk("src/Widget.cs", src);
        assert_eq!(item(&items, "Acme.Widgets").kind, "module");
        let w = item(&items, "Widget");
        assert_eq!((w.kind.as_str(), w.doc.as_str()), ("class", "A widget."));
        assert_eq!(w.qualified_name, "Acme.Widgets::Widget");
        let count = item(&items, "Count");
        assert_eq!(
            (count.kind.as_str(), count.doc.as_str()),
            ("property", "The count.")
        );
        assert_eq!(item(&items, "Mode").kind, "type");
        assert_eq!(item(&items, "IRunnable").kind, "interface");
        assert!(item(&items, "Param").is_test);
        assert!(!item(&items, "Helper").is_test);
    }

    #[cfg(feature = "lang-ruby")]
    #[test]
    fn ruby_definitions_docs_and_tests() {
        let src = br#"# A widget.
class Widget < Base
  # Make one.
  def initialize(n)
    @n = n
  end

  def self.build; end
end

class WidgetTest < Minitest::Test
  def test_runs
  end
end
"#;
        let items = chunker().chunk("lib/widget.rb", src);
        let w = item(&items, "Widget");
        assert_eq!((w.kind.as_str(), w.doc.as_str()), ("class", "A widget."));
        let init = item(&items, "initialize");
        assert_eq!(init.doc, "Make one.");
        assert_eq!(init.signature, "def initialize(n)");
        assert_eq!(item(&items, "build").qualified_name, "Widget::build");
        assert!(item(&items, "test_runs").is_test);
        assert!(!w.is_test);
    }

    #[cfg(feature = "lang-clarity")]
    #[test]
    fn clarity_definitions_with_docs() {
        let src = br#";; Token contract (file header, not a doc).

;; Raised when the caller is not the owner.
(define-constant ERR_NOT_OWNER (err u100))
(define-constant ERR_ALIAS ERR_NOT_OWNER)
(define-data-var owner principal tx-sender)
;; Balances by holder.
(define-map balances principal uint)
(define-fungible-token stx-token)
(define-non-fungible-token badge uint)

(define-trait transferable
  ((transfer (uint principal) (response bool uint))))

;; Move `amount` from the caller to `to`.
;; Fails unless the caller holds enough.
(define-public (transfer (amount uint) (to principal))
  (ok true))

(define-read-only (get-balance (who principal))
  (default-to u0 (map-get? balances who)))

(define-private (is-owner)
  (is-eq tx-sender (var-get owner)))
"#;
        let items = chunker().chunk("contracts/token.clar", src);
        let kinds: Vec<_> = items
            .iter()
            .filter(|i| i.tagged)
            .map(|i| (i.kind.as_str(), i.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("constant", "ERR_NOT_OWNER"),
                ("constant", "ERR_ALIAS"),
                ("variable", "owner"),
                ("map", "balances"),
                ("token", "stx-token"),
                ("token", "badge"),
                ("interface", "transferable"),
                ("method", "transfer"),
                ("function", "transfer"),
                ("function", "get-balance"),
                ("function", "is-owner"),
            ]
        );
        let by = |kind: &str, name: &str| {
            items
                .iter()
                .find(|i| i.kind == kind && i.name == name)
                .unwrap()
        };
        assert_eq!(
            by("constant", "ERR_NOT_OWNER").doc,
            "Raised when the caller is not the owner."
        );
        assert_eq!(by("map", "balances").doc, "Balances by holder.");
        let transfer = by("function", "transfer");
        assert_eq!(
            transfer.doc,
            "Move `amount` from the caller to `to`.\nFails unless the caller holds enough."
        );
        assert_eq!((transfer.line_start, transfer.line_end), (17, 18));
        assert_eq!(by("function", "get-balance").doc, "");
        assert_eq!(
            by("method", "transfer").qualified_name,
            "transferable::transfer"
        );
    }

    #[test]
    fn name_positions_are_one_based_characters() {
        let src = "/// Doc.\n#[derive(Debug)]\npub struct Reader {}\n\nimpl Reader {\n    pub fn new() -> Self { Reader {} }\n}\n/* é */ fn after_unicode() {}\n";
        let items = chunker().chunk("a.rs", src.as_bytes());
        let pos = |n: &str| {
            let i = items.iter().find(|i| i.name == n).unwrap();
            (i.line_start, i.name_line, i.name_column)
        };
        assert_eq!(pos("Reader"), (3, 3, 12));
        assert_eq!(pos("new"), (6, 6, 12));
        assert_eq!(pos("after_unicode"), (8, 8, 12));
        let py = chunker().chunk("a.py", b"class A:\n    def run(self):\n        pass\n");
        let run = py.iter().find(|i| i.name == "run").unwrap();
        assert_eq!((run.name_line, run.name_column), (2, 9));
        let text = chunker().chunk("notes.txt", b"\n\n  Indented first line.\nMore.\n");
        assert_eq!((text[0].name_line, text[0].name_column), (3, 3));
    }

    #[test]
    fn doc_text_is_header_and_doc() {
        let items = chunker().chunk(
            "a.rs",
            b"/// Copy a tree.\npub fn copy_tree() {}\nfn bare() {}\n",
        );
        let find = |n: &str| items.iter().find(|i| i.name == n).unwrap();
        assert_eq!(
            find("copy_tree").doc_text().as_deref(),
            Some("function copy_tree\nCopy a tree.")
        );
        assert_eq!(find("bare").doc_text(), None);
    }

    #[test]
    fn container_text_collapses_members() {
        let mut src = String::from("impl Foo {\n");
        for i in 0..50 {
            src.push_str(&format!(
                "    pub fn m{i}(&self) -> u32 {{\n        {i}\n    }}\n"
            ));
        }
        src.push_str("}\n");
        let items = chunker().chunk("a.rs", src.as_bytes());
        let imp = items.iter().find(|i| i.kind == "impl").unwrap();
        assert!(imp.text.contains("pub fn m0(&self) -> u32 …"));
        assert!(!imp.text.contains("        0\n"));
    }

    #[test]
    fn markdown_sections_and_text_fallback() {
        let items = chunker().chunk("README.md", b"# Title\n\nIntro.\n\n## Usage\n\nRun it.\n");
        assert!(
            items
                .iter()
                .any(|i| i.kind == "section" && i.name == "Usage")
        );
        let mut c = chunker();
        c.max_chars = 40;
        let reg_items = c.chunk("notes.unknownext", b"hello");
        assert!(reg_items.is_empty());
    }

    #[test]
    fn module_chunks_are_opt_in() {
        let src =
            b"//! Time helpers.\n\npub fn now_secs() -> u64 { 0 }\npub fn now_ms() -> u64 { 0 }\n";
        assert!(
            chunker()
                .chunk("util/time.rs", src)
                .iter()
                .all(|i| i.kind != "module")
        );
        let items = chunker()
            .with_module_chunks(true)
            .chunk("util/time.rs", src);
        let m = items.iter().find(|i| i.kind == "module").unwrap();
        assert_eq!(m.name, "time");
        assert!(m.text.contains("//! Time helpers.") && m.text.contains("pub fn now_ms() -> u64"));
    }

    #[test]
    fn signature_heuristic() {
        assert_eq!(
            signature("fn a(x: u8) -> Vec<u8>\nwhere T: X\n{ body }"),
            "fn a(x: u8) -> Vec<u8> where T: X"
        );
        assert_eq!(signature("def f(a, b):\n    return a"), "def f(a, b)");
        assert_eq!(signature("# Heading\n\nbody text"), "# Heading");
        assert_eq!(signature("const X: u64 = 5;"), "const X: u64 = 5");
        assert_eq!(
            signature("def initialize(n)\n  @n = n\nend"),
            "def initialize(n)"
        );
        assert_eq!(signature("def self.build; end"), "def self.build");
    }

    #[cfg(feature = "lang-svelte")]
    #[test]
    fn svelte_scripts_are_tagged_and_markup_is_text() {
        let src = "<script context=\"module\" lang=\"ts\">
  export const PAGE = 20;
</script>

<script lang=\"ts\">
  let { rows } = $props();
  /** Thousands as `k`/`M` (é). */
  const fmt = (k: number) => (k >= 1000 ? `${k / 1000}M` : `${k}k`);
  function total(r: number[]): number {
    return r.reduce((a, b) => a + b, 0);
  }
</script>

<h1>Chart</h1>
<ul>{#each rows as r}<li>{fmt(total(r))}</li>{/each}</ul>

<style>
  h1 { color: rebeccapurple; }
</style>
";
        let items = chunker().chunk("src/lib/Chart.svelte", src.as_bytes());
        assert!(items.iter().all(|i| i.language == "svelte"), "{items:#?}");
        let def = |name: &str| items.iter().find(|i| i.name == name && i.tagged);
        let fmt = def("fmt").expect("fmt");
        assert_eq!((fmt.line_start, fmt.name_line, fmt.name_column), (8, 8, 9));
        assert!(fmt.doc.contains("Thousands"), "{fmt:?}");
        let total = def("total").expect("total");
        assert_eq!((total.line_start, total.line_end), (9, 11));
        assert!(total.text.contains("reduce"));
        let markup: Vec<_> = items.iter().filter(|i| !i.tagged).collect();
        assert!(
            markup.iter().any(|i| i.text.contains("<h1>Chart</h1>")),
            "{markup:#?}"
        );
        assert!(markup.iter().all(|i| !i.text.contains("rebeccapurple")
            && !i.text.contains("reduce")
            && !i.text.contains("<script")));
        assert_eq!(markup[0].line_start, 14);
    }

    #[cfg(feature = "lang-svelte")]
    #[test]
    fn svelte_plain_script_is_javascript_and_scriptless_files_are_markup() {
        let js = b"<script>\n  function go(href) { location.href = href; }\n</script>\n<a on:click={() => go('/')}>Home</a>\n";
        let items = chunker().chunk("Nav.svelte", js);
        assert!(
            items.iter().any(|i| i.name == "go" && i.tagged),
            "{items:#?}"
        );
        let items = chunker().chunk("Static.svelte", b"<p>Nothing to run.</p>\n");
        assert_eq!(items.len(), 1);
        assert!(!items[0].tagged && items[0].text.contains("Nothing to run."));
    }

    #[test]
    fn long_paragraphs_are_cut_at_lines() {
        let line = "x".repeat(300);
        let src = format!("{line}\n{line}\n{line}\n{line}\n\nshort\n");
        let items = Chunker::new(chunker().registry().clone(), 200).chunk("a.txt", src.as_bytes());
        // 800-character budget: two 300-character lines per item, and `short` joins the last.
        let lines: Vec<_> = items.iter().map(|i| (i.line_start, i.line_end)).collect();
        assert_eq!(lines, [(1, 2), (3, 6)], "{items:#?}");
    }
}
