//! The output format (one line per item). This format is the product; keep it stable:
//! `rel_path:line  signature — first doc line`, signature ≤ 120 chars, doc ≤ 100.

use std::path::Path;

use crate::chunk::Item;

const SIG_MAX: usize = 120;
const DOC_MAX: usize = 100;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// The signature with the bare name replaced by the qualified name, or `kind qualified_name`
/// when the signature does not mention the name.
pub fn display_signature(item: &Item) -> String {
    let sig = &item.signature;
    if let Some(pos) = find_word(sig, &item.name) {
        if item.qualified_name != item.name {
            return format!(
                "{}{}{}",
                &sig[..pos],
                item.qualified_name,
                &sig[pos + item.name.len()..]
            );
        }
        return sig.clone();
    }
    format!("{} {}", item.kind, item.qualified_name)
}

fn find_word(hay: &str, word: &str) -> Option<usize> {
    if word.is_empty() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = hay[from..].find(word) {
        let start = from + i;
        let end = start + word.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = hay[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return Some(start);
        }
        from = end;
    }
    None
}

pub fn line(item: &Item) -> String {
    let sig = clip(&display_signature(item), SIG_MAX);
    let doc = item.doc_first_line();
    if doc.is_empty() {
        format!("{}:{}  {}", item.rel_path, item.line_start, sig)
    } else {
        format!(
            "{}:{}  {}  — {}",
            item.rel_path,
            item.line_start,
            sig,
            clip(doc, DOC_MAX)
        )
    }
}

/// The first `n` source lines of the item, read from the worktree.
pub fn body(root: &Path, item: &Item, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(root.join(&item.rel_path)) else {
        return String::new();
    };
    text.lines()
        .skip(item.line_start.saturating_sub(1) as usize)
        .take(n.min((item.line_end + 1).saturating_sub(item.line_start) as usize))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, qualified: &str, sig: &str, doc: &str) -> Item {
        Item {
            rel_path: "stacks-common/src/util/retry.rs".into(),
            language: "rust".into(),
            kind: "function".into(),
            name: name.into(),
            qualified_name: qualified.into(),
            line_start: 19,
            line_end: 30,
            signature: sig.into(),
            doc: doc.into(),
            is_test: false,
            tagged: true,
            text: String::new(),
        }
    }

    #[test]
    fn plan_example_lines() {
        let i = item(
            "RetryReader",
            "RetryReader",
            "struct RetryReader<'a, R: Read>",
            "Wrap a Read so that we store a copy of what was read\nmore",
        );
        assert_eq!(
            line(&i),
            "stacks-common/src/util/retry.rs:19  struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read"
        );
        let m = item(
            "since",
            "ClarityVersion::since",
            "pub fn since(v: ClarityVersion)",
            "",
        );
        assert_eq!(
            line(&m),
            "stacks-common/src/util/retry.rs:19  pub fn ClarityVersion::since(v: ClarityVersion)"
        );
        let long = item(
            "f",
            "f",
            &format!("fn f({})", "x".repeat(200)),
            &"d".repeat(150),
        );
        let l = line(&long);
        assert!(l.contains('…'));
        let sig_part = l.split("  ").nth(1).unwrap();
        assert_eq!(sig_part.chars().count(), 120);
    }
}
