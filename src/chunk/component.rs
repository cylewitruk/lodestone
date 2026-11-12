//! Single-file components (Svelte's `.svelte`): markup with embedded `<script>` and `<style>`
//! blocks. The component's grammar only locates the blocks. Script blocks are tagged with the
//! TypeScript or JavaScript grammar (as their `lang` says), the markup is chunked as text, and
//! styles are left out.
//!
//! Each part is a copy of the file with everything else blanked byte for byte (newlines kept), so
//! the usual chunking applies unchanged and every offset and line is the file's own.

use std::ops::Range;

use tree_sitter::{Language as Grammar, Node, Parser};

/// A component file format.
pub struct Component {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub grammar: Grammar,
}

/// A component file split into what is chunked.
pub struct Parts {
    /// The script blocks' contents, everything else blanked.
    pub script: Vec<u8>,
    /// Whether a script block is TypeScript (`lang="ts"`); otherwise JavaScript.
    pub typescript: bool,
    /// The markup: script and style blocks blanked.
    pub markup: Vec<u8>,
}

impl Component {
    /// Splits `source`; `None` when it does not parse.
    pub fn split(&self, parser: &mut Parser, source: &[u8]) -> Option<Parts> {
        parser.set_language(&self.grammar).ok()?;
        let tree = parser.parse(source, None)?;
        let mut scripts = Vec::new();
        let mut blocks = Vec::new();
        let mut typescript = false;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "script_element" => {
                    blocks.push(node.byte_range());
                    if let Some(body) = child(node, "raw_text") {
                        scripts.push(body.byte_range());
                    }
                    typescript |= child(node, "start_tag")
                        .and_then(|tag| attribute(tag, "lang", source))
                        .is_some_and(|l| l == "ts" || l == "typescript");
                }
                "style_element" => blocks.push(node.byte_range()),
                _ => {
                    let mut cursor = node.walk();
                    stack.extend(node.named_children(&mut cursor));
                }
            }
        }
        Some(Parts {
            script: keep(source, &scripts),
            typescript,
            markup: blank(source, &blocks),
        })
    }
}

fn child<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|c| c.kind() == kind)
}

/// The value of attribute `name` on a start tag.
fn attribute<'s>(tag: Node, name: &str, source: &'s [u8]) -> Option<&'s str> {
    let mut cursor = tag.walk();
    tag.named_children(&mut cursor)
        .filter(|a| a.kind() == "attribute")
        .find(|a| child(*a, "attribute_name").is_some_and(|n| text(n, source) == Some(name)))
        .and_then(|a| {
            child(a, "quoted_attribute_value")
                .and_then(|q| child(q, "attribute_value"))
                .or_else(|| child(a, "attribute_value"))
        })
        .and_then(|v| text(v, source))
}

fn text<'s>(node: Node, source: &'s [u8]) -> Option<&'s str> {
    std::str::from_utf8(&source[node.byte_range()]).ok()
}

/// `source` with only `ranges` kept.
fn keep(source: &[u8], ranges: &[Range<usize>]) -> Vec<u8> {
    let mut out: Vec<u8> = source.iter().map(|&b| blanked(b)).collect();
    for r in ranges {
        out[r.clone()].copy_from_slice(&source[r.clone()]);
    }
    out
}

/// `source` with `ranges` blanked.
fn blank(source: &[u8], ranges: &[Range<usize>]) -> Vec<u8> {
    let mut out = source.to_vec();
    for r in ranges {
        for b in &mut out[r.clone()] {
            *b = blanked(*b);
        }
    }
    out
}

/// Line breaks stay, so lines keep their numbers; everything else becomes a space.
fn blanked(b: u8) -> u8 {
    if matches!(b, b'\n' | b'\r') { b } else { b' ' }
}
