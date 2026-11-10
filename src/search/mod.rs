//! Hybrid search over one snapshot: kNN (code and doc vectors) + BM25 → reciprocal rank fusion → result-only filters →
//! one row per item → optional rerank → top-k. Never touches index state.

pub mod format;
pub mod rerank;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::SearchConfig;
use crate::index::{ItemRow, Lexical, Snapshot};

/// Result-only scope filters (invariant 5: they never touch index state).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Filters {
    pub from_unit: Option<String>,
    pub path_prefix: Option<String>,
    pub language: Option<String>,
    pub kinds: Vec<String>,
    pub include_tests: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub row: Arc<ItemRow>,
    /// Fused score (higher is better; not comparable across queries).
    pub score: f32,
    /// Cosine similarity to the query, when the item was a vector candidate.
    pub similarity: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub hits: Vec<Hit>,
    pub stale: bool,
    pub notes: Vec<String>,
}

/// Sentences of a multi-sentence query worth embedding on their own (at least `min_words`
/// words). Returns nothing for single-sentence queries.
pub fn sentences(query: &str, min_words: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = query.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let end = matches!(c, '.' | '?' | '!' | ';')
            && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if end {
            out.push(std::mem::take(&mut cur));
        }
    }
    out.push(cur);
    let out: Vec<String> = out
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| s.split_whitespace().count() >= min_words)
        .collect();
    if out.len() < 2 { Vec::new() } else { out }
}

/// An identifier-looking query token: `snake_case`, `CamelCase` (inner capital) or a path.
pub fn identifier_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .flat_map(|t| t.split("::"))
        .filter(|t| {
            let t = t.trim_matches(':');
            t.len() > 2
                && (t.contains('_') && t.chars().any(|c| c.is_alphabetic())
                    || t.chars().skip(1).any(|c| c.is_uppercase())
                        && t.chars().any(|c| c.is_lowercase()))
        })
        .map(|t| t.trim_matches(':').to_string())
        .collect()
}

pub struct SearchParams<'a> {
    pub query: &'a str,
    pub query_vector: Option<&'a [f32]>,
    /// Vectors of the query's individual sentences (see [`sentences`]), fused at
    /// `sentence_weight`.
    pub sentence_vectors: &'a [Vec<f32>],
    pub filters: &'a Filters,
    pub top_k: usize,
    pub config: &'a SearchConfig,
}

/// Runs the hybrid pipeline on `snap` (without rerank; see [`rerank`]).
pub fn search(snap: &Snapshot, lexical: &Lexical, p: &SearchParams) -> anyhow::Result<Outcome> {
    let mut notes = Vec::new();
    let k = p.config.candidates.max(p.top_k);
    let vector_hits: Vec<(u64, f32)> = match p.query_vector {
        Some(q) if p.config.vector_weight <= 0.0 && snap.vectors.dim() == q.len() => Vec::new(),
        Some(q) if snap.vectors.dim() == q.len() => snap
            .vectors
            .search(q, k)?
            .into_iter()
            .filter(|(id, _)| snap.items.contains_key(id))
            .collect(),
        Some(_) => {
            notes.push("query vector dimension does not match index; lexical only".into());
            Vec::new()
        }
        None => Vec::new(),
    };
    let doc_hits: Vec<(u64, f32)> = match p.query_vector {
        Some(q) if p.config.doc_weight > 0.0 && snap.doc_vectors.dim() == q.len() => snap
            .doc_vectors
            .search(q, k)?
            .into_iter()
            .filter(|(id, _)| snap.items.contains_key(id))
            .collect(),
        _ => Vec::new(),
    };
    let mut sentence_hits: Vec<Vec<(u64, f32)>> = Vec::new();
    for v in p
        .sentence_vectors
        .iter()
        .filter(|v| v.len() == snap.vectors.dim())
    {
        sentence_hits.push(
            snap.vectors
                .search(v, k)?
                .into_iter()
                .filter(|(id, _)| snap.items.contains_key(id))
                .collect(),
        );
    }
    let idents = identifier_tokens(p.query);
    let lex_weight = if idents.is_empty() {
        p.config.lexical_weight
    } else {
        p.config.identifier_lexical_weight
    };
    // A list weighted 0 contributes no candidates at all.
    let lexical_hits: Vec<(u64, f32)> = if lex_weight > 0.0 {
        lexical
            .search(&snap.searcher, p.query, k)?
            .into_iter()
            .filter(|(id, _)| snap.items.contains_key(id))
            .collect()
    } else {
        Vec::new()
    };

    let mut fused: HashMap<u64, (f32, Option<f32>)> = HashMap::new();
    for (hits, weight) in [
        (&vector_hits, p.config.vector_weight),
        (&doc_hits, p.config.doc_weight),
    ] {
        if weight <= 0.0 {
            continue;
        }
        for (rank, (id, sim)) in hits.iter().enumerate() {
            let e = fused.entry(*id).or_insert((0.0, None));
            e.0 += weight / (p.config.rrf_k + rank as f32 + 1.0);
            e.1 = Some(e.1.map_or(*sim, |s: f32| s.max(*sim)));
        }
    }
    for hits in &sentence_hits {
        for (rank, (id, _)) in hits.iter().enumerate() {
            fused.entry(*id).or_insert((0.0, None)).0 +=
                p.config.sentence_weight / (p.config.rrf_k + rank as f32 + 1.0);
        }
    }
    for (rank, (id, _)) in lexical_hits.iter().enumerate() {
        fused.entry(*id).or_insert((0.0, None)).0 +=
            lex_weight / (p.config.rrf_k + rank as f32 + 1.0);
    }

    let allowed_units: Option<BTreeSet<String>> = match &p.filters.from_unit {
        Some(u) if snap.graph.has_edges() => match snap.graph.allowed_from(u) {
            Some(set) => Some(set),
            None => {
                notes.push(format!("unknown unit {u}; from_unit ignored"));
                None
            }
        },
        Some(_) => {
            notes.push("no build graph provider matched; from_unit ignored".into());
            None
        }
        None => None,
    };
    let f = p.filters;
    let mut hits: Vec<Hit> = fused
        .into_iter()
        .filter_map(|(id, (score, similarity))| {
            let row = snap.items.get(&id)?;
            let it = &row.item;
            if let Some(allowed) = &allowed_units
                && !row.unit.as_ref().is_some_and(|u| allowed.contains(u))
            {
                return None;
            }
            if let Some(prefix) = &f.path_prefix
                && !it.rel_path.starts_with(prefix.as_str())
            {
                return None;
            }
            if let Some(lang) = &f.language
                && !it.language.eq_ignore_ascii_case(lang)
            {
                return None;
            }
            if !f.kinds.is_empty() && !f.kinds.iter().any(|k| k == &it.kind) {
                return None;
            }
            if it.is_test && !f.include_tests {
                return None;
            }
            let mut score = score;
            if it.is_test {
                score *= p.config.test_weight;
            }
            if p.config.exact_identifier_first
                && idents
                    .iter()
                    .any(|t| t == &it.name || t == &it.qualified_name)
            {
                score += 10.0;
            }
            Some(Hit {
                row: row.clone(),
                score,
                similarity,
            })
        })
        .collect();
    // Deterministic order: score, then path/line.
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.row.item.rel_path.cmp(&b.row.item.rel_path))
            .then_with(|| a.row.item.line_start.cmp(&b.row.item.line_start))
    });
    // One row per item location (identical definitions indexed twice collapse).
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| {
        seen.insert((
            h.row.item.rel_path.clone(),
            h.row.item.line_start,
            h.row.item.name.clone(),
        ))
    });
    Ok(Outcome {
        hits,
        stale: !snap.stale.is_empty(),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_split() {
        assert!(sentences("copy a directory tree recursively", 4).is_empty());
        let s = sentences(
            "I need the current time in seconds. Is there already something for that? e.g. now.",
            4,
        );
        assert_eq!(
            s,
            vec![
                "I need the current time in seconds.",
                "Is there already something for that?"
            ]
        );
    }

    #[test]
    fn identifier_detection() {
        assert_eq!(
            identifier_tokens("where is u64_to_sql used"),
            vec!["u64_to_sql"]
        );
        assert_eq!(
            identifier_tokens("RetryReader and std::io::Read"),
            vec!["RetryReader"]
        );
        assert!(identifier_tokens("recursively copy a directory").is_empty());
    }
}
