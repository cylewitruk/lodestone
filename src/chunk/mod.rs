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
pub use languages::{Language, LanguageInfo, LanguageRegistry};

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
    /// Embed only header + doc + signature (experiment; see CHANGELOG).
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

    /// Changes whenever chunk output for unchanged files could change.
    pub fn fingerprint(&self) -> String {
        let key = format!(
            "{ITEM_FORMAT}|{}|{}|{}|{}",
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
        if let Some(c) = self.registry.component_for(path) {
            return self.chunk_component(rel_path, c, source);
        }
        if let Some(lang) = self.registry.for_path(path) {
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
        let guest = if parts.typescript {
            "typescript"
        } else {
            "javascript"
        };
        let mut items = self
            .registry
            .by_name(guest)
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
            markup
                .iter()
                .any(|i| i.text.contains("<h1>Chart</h1>")),
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
