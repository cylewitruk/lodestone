//! Chunking: one item per definition as identified by the grammar's tags query; files without a
//! grammar (or without any definitions) are chunked into prose paragraphs.

pub mod languages;

use std::cell::RefCell;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tree_sitter::{Parser, QueryCursor, StreamingIterator};
use tree_sitter_tags::TagsContext;

pub use languages::{Language, LanguageInfo, LanguageRegistry};

/// Kind assigned to prose chunks.
pub const TEXT_KIND: &str = "text";
/// Language assigned to prose chunks from files without a grammar.
pub const TEXT_LANGUAGE: &str = "text";

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
    /// Embed only header + doc + signature (experiment; see CHANGELOG).
    summary_only: bool,
}

struct Def {
    kind: String,
    name: String,
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

    /// Changes whenever chunk output for unchanged files could change.
    pub fn fingerprint(&self) -> String {
        let key = format!(
            "{}|{}|{}|{}",
            self.registry.fingerprint(),
            self.max_chars,
            self.module_chunks,
            self.summary_only
        );
        blake3::hash(key.as_bytes()).to_hex()[..16].to_string()
    }

    pub fn registry(&self) -> &Arc<LanguageRegistry> {
        &self.registry
    }

    /// Chunks one file. Returns no items for files the registry does not accept.
    pub fn chunk(&self, rel_path: &str, source: &[u8]) -> Vec<Item> {
        let path = Path::new(rel_path);
        if let Some(lang) = self.registry.for_path(path) {
            let items = self.chunk_tagged(rel_path, lang, source);
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

    fn chunk_tagged(&self, rel_path: &str, lang: &Language, source: &[u8]) -> Vec<Item> {
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
                    &lang.name,
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
                Item {
                    rel_path: rel_path.to_string(),
                    language: lang.name.clone(),
                    kind: d.kind.clone(),
                    name: d.name.clone(),
                    qualified_name: qualified[i].clone(),
                    line_start: lines.line_of(d.range.start),
                    line_end: lines.line_of(d.range.end.saturating_sub(1).max(d.range.start)),
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

    /// Blank-line-separated paragraphs merged up to the chunk budget.
    fn chunk_text(&self, rel_path: &str, language: &str, source: &[u8]) -> Vec<Item> {
        let text = String::from_utf8_lossy(source);
        let mut items = Vec::new();
        let mut buf = String::new();
        let mut start_line = 0u32;
        let mut para = String::new();
        let mut para_start = 0u32;
        let flush = |buf: &mut String, start: u32, end: u32, items: &mut Vec<Item>| {
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                buf.clear();
                return;
            }
            let name: String = trimmed
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(80)
                .collect();
            items.push(Item {
                rel_path: rel_path.to_string(),
                language: language.to_string(),
                kind: TEXT_KIND.to_string(),
                name: name.clone(),
                qualified_name: name.clone(),
                line_start: start,
                line_end: end.max(start),
                signature: name,
                doc: String::new(),
                is_test: false,
                tagged: false,
                text: truncate(
                    &format!("{rel_path} {TEXT_KIND}\n{trimmed}"),
                    self.max_chars,
                ),
            });
            buf.clear();
        };
        let all_lines: Vec<&str> = text.lines().collect();
        for (i, line) in all_lines.iter().enumerate() {
            let lineno = i as u32 + 1;
            if line.trim().is_empty() {
                if !para.is_empty() {
                    if !buf.is_empty() && buf.len() + para.len() > self.max_chars {
                        flush(&mut buf, start_line, para_start - 1, &mut items);
                    }
                    if buf.is_empty() {
                        start_line = para_start;
                    }
                    buf.push_str(&para);
                    buf.push('\n');
                    para.clear();
                }
                continue;
            }
            if para.is_empty() {
                para_start = lineno;
            }
            para.push_str(line);
            para.push('\n');
        }
        if !para.is_empty() {
            if !buf.is_empty() && buf.len() + para.len() > self.max_chars {
                flush(&mut buf, start_line, para_start - 1, &mut items);
            }
            if buf.is_empty() {
                start_line = para_start;
            }
            buf.push_str(&para);
        }
        flush(&mut buf, start_line, all_lines.len() as u32, &mut items);
        items
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
/// when a blank line comes first.
pub fn signature(def_text: &str) -> String {
    let mut depth: i32 = 0;
    let mut end = def_text.len();
    let bytes = def_text.as_bytes();
    let mut i = 0;
    let mut prev = b' ';
    while i < bytes.len() && i < 600 {
        let b = bytes[i];
        match b {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'<' => depth += 1,
            b'>' if prev != b'-' && prev != b'=' => depth -= 1,
            b'{' | b';' if depth <= 0 => {
                end = i;
                break;
            }
            b':' if depth <= 0 => {
                let rest = &def_text[i + 1..];
                if rest.starts_with('\n') || rest.starts_with("\r\n") || rest.is_empty() {
                    end = i;
                    break;
                }
            }
            b'\n' => {
                let rest = &def_text[i + 1..];
                if rest.trim_start_matches([' ', '\t', '\r']).starts_with('\n') {
                    end = def_text.find('\n').unwrap_or(def_text.len());
                    break;
                }
            }
            _ => {}
        }
        prev = b;
        i += 1;
    }
    if i >= 600 {
        end = def_text.find('\n').unwrap_or(def_text.len());
    }
    let mut s = slice(def_text, 0..end)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if s.chars().count() > 240 {
        s = s.chars().take(240).collect();
    }
    s
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
    }
}
