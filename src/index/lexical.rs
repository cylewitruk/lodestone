//! tantivy BM25 index over name, signature, doc and body, keyed by item id.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tantivy::collector::{FilterCollector, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term, doc,
};

use super::items::ItemRow;

/// Lexical tokenizer choice (an index-time setting).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TokenizerKind {
    /// Splits identifiers on `snake_case`/`CamelCase` boundaries and keeps the whole identifier.
    #[default]
    Code,
    /// tantivy's simple tokenizer + lowercasing.
    Default,
}

const TOKENIZER: &str = "lodestone";

/// Splits text into identifier runs; each run emits its lowercase sub-words and, when it has
/// more than one, the whole run at the first sub-word's position.
#[derive(Clone, Default)]
pub struct CodeTokenizer {
    tokens: Vec<Token>,
}

pub struct VecTokenStream<'a> {
    tokens: &'a [Token],
    idx: usize,
}

impl TokenStream for VecTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.idx += 1;
        self.idx <= self.tokens.len()
    }
    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        unreachable!("lodestone code tokens are not filtered in place")
    }
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = VecTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream<'a> {
        self.tokens.clear();
        let mut position = 0usize;
        for (start, run) in identifier_runs(text) {
            let parts = split_identifier(run);
            if parts.len() > 1 && run.len() <= 64 {
                self.tokens.push(Token {
                    offset_from: start,
                    offset_to: start + run.len(),
                    position,
                    text: run.to_lowercase(),
                    position_length: 1,
                });
            }
            for (off, part) in parts {
                if part.len() > 40 {
                    continue;
                }
                self.tokens.push(Token {
                    offset_from: start + off,
                    offset_to: start + off + part.len(),
                    position,
                    text: part.to_lowercase(),
                    position_length: 1,
                });
                position += 1;
            }
        }
        VecTokenStream {
            tokens: &self.tokens,
            idx: 0,
        }
    }
}

fn identifier_runs(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, &text[s..i]));
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out.into_iter()
}

/// `get_HTTPServer_v2` → `get`, `HTTP`, `Server`, `v2` with byte offsets.
pub fn split_identifier(run: &str) -> Vec<(usize, &str)> {
    let mut parts = Vec::new();
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    let mut start: Option<usize> = None;
    for (k, &(i, c)) in chars.iter().enumerate() {
        if c == '_' {
            if let Some(s) = start.take() {
                parts.push((s, &run[s..i]));
            }
            continue;
        }
        if let Some(s) = start {
            let prev = chars[k - 1].1;
            let next = chars.get(k + 1).map(|x| x.1);
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(|n| n.is_lowercase()));
            if boundary {
                parts.push((s, &run[s..i]));
                start = Some(i);
            }
        } else {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        parts.push((s, &run[s..]));
    }
    parts
}

/// Words that carry no signal in natural-language code queries.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "of",
    "to",
    "in",
    "on",
    "for",
    "with",
    "is",
    "are",
    "be",
    "it",
    "its",
    "this",
    "that",
    "there",
    "i",
    "we",
    "my",
    "me",
    "need",
    "want",
    "some",
    "so",
    "as",
    "at",
    "by",
    "from",
    "into",
    "if",
    "do",
    "does",
    "any",
    "already",
    "something",
    "repo",
    "repository",
    "have",
    "has",
    "can",
    "which",
    "what",
    "how",
    "all",
    "each",
    "every",
    "one",
    "like",
    "e",
    "g",
    "eg",
    "not",
    "but",
    "than",
    "then",
    "them",
    "they",
    "would",
    "should",
    "could",
    "will",
    "just",
    "only",
    "get",
    "given",
    "whether",
];

#[derive(Clone, Copy)]
pub struct Fields {
    pub id: Field,
    pub rel_path: Field,
    pub name: Field,
    pub signature: Field,
    pub doc: Field,
    pub body: Field,
}

pub struct Lexical {
    index: Index,
    reader: IndexReader,
    pub fields: Fields,
    tokenizer: TokenizerKind,
}

fn schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let fields = Fields {
        id: sb.add_u64_field("id", INDEXED | STORED | FAST),
        rel_path: sb.add_text_field("rel_path", STRING),
        name: sb.add_text_field("name", text.clone()),
        signature: sb.add_text_field("signature", text.clone()),
        doc: sb.add_text_field("doc", text.clone()),
        body: sb.add_text_field("body", text),
    };
    (sb.build(), fields)
}

fn register(index: &Index, kind: TokenizerKind) {
    let analyzer = match kind {
        TokenizerKind::Code => TextAnalyzer::builder(CodeTokenizer::default()).build(),
        TokenizerKind::Default => TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .build(),
    };
    index.tokenizers().register(TOKENIZER, analyzer);
}

impl Lexical {
    pub fn open(dir: &Path, kind: TokenizerKind) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = schema();
        let index = match Index::open_in_dir(dir) {
            Ok(i) => i,
            Err(_) => {
                // Missing or unreadable: start fresh (lexical state is rebuildable from items).
                for e in std::fs::read_dir(dir)?.flatten() {
                    let _ = std::fs::remove_file(e.path());
                }
                Index::create_in_dir(dir, schema)?
            }
        };
        register(&index, kind);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            fields,
            tokenizer: kind,
        })
    }

    pub fn writer(&self) -> anyhow::Result<IndexWriter> {
        Ok(self.index.writer(64_000_000)?)
    }

    pub fn reload(&self) -> anyhow::Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }

    pub fn delete_paths(&self, writer: &IndexWriter, paths: &[String]) {
        for p in paths {
            writer.delete_term(Term::from_field_text(self.fields.rel_path, p));
        }
    }

    pub fn add(&self, writer: &IndexWriter, row: &ItemRow) -> anyhow::Result<()> {
        let f = &self.fields;
        writer.add_document(doc!(
            f.id => row.id,
            f.rel_path => row.item.rel_path.as_str(),
            f.name => format!("{} {}", row.item.name, row.item.qualified_name),
            f.signature => row.item.signature.as_str(),
            f.doc => row.item.doc.as_str(),
            f.body => row.item.text.as_str(),
        ))?;
        Ok(())
    }

    /// Query terms after tokenization and stop-word removal.
    pub fn query_terms(&self, query: &str) -> Vec<String> {
        let mut analyzer = match self.tokenizer {
            TokenizerKind::Code => TextAnalyzer::builder(CodeTokenizer::default()).build(),
            TokenizerKind::Default => TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(LowerCaser)
                .build(),
        };
        let mut stream = analyzer.token_stream(query);
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while stream.advance() {
            let t = stream.token().text.clone();
            if t.len() > 1 && !STOPWORDS.contains(&t.as_str()) && seen.insert(t.clone()) {
                out.push(t);
            }
        }
        out
    }

    /// BM25 over all text fields with field boosts, over documents whose id `admit` accepts;
    /// returns (id, score), best first.
    pub fn search(
        &self,
        searcher: &Searcher,
        query: &str,
        k: usize,
        admit: impl Fn(u64) -> bool + Clone + Send + Sync + 'static,
    ) -> anyhow::Result<Vec<(u64, f32)>> {
        let terms = self.query_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let f = self.fields;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for t in &terms {
            for (field, boost) in [
                (f.name, 3.0),
                (f.signature, 1.5),
                (f.doc, 1.5),
                (f.body, 1.0),
            ] {
                let q = TermQuery::new(
                    Term::from_field_text(field, t),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(q), boost))));
            }
        }
        let q = BooleanQuery::new(clauses);
        let top = searcher.search(
            &q,
            &FilterCollector::new(
                "id".to_string(),
                admit,
                TopDocs::with_limit(k).order_by_score(),
            ),
        )?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let seg = searcher.segment_reader(addr.segment_ord);
            if let Some(id) = seg.fast_fields().u64("id")?.first(addr.doc_id) {
                out.push((id, score));
            }
        }
        Ok(out)
    }

    pub fn num_docs(searcher: &Searcher) -> u64 {
        searcher.num_docs()
    }

    /// Best-effort read of a stored document's id, for consistency checks in tests.
    pub fn doc_id(searcher: &Searcher, addr: tantivy::DocAddress, f: &Fields) -> Option<u64> {
        use tantivy::schema::Value;
        let d: TantivyDocument = searcher.doc(addr).ok()?;
        d.get_first(f.id).and_then(|v| v.as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_identifiers() {
        let parts: Vec<&str> = split_identifier("get_HTTPServer_v2")
            .into_iter()
            .map(|p| p.1)
            .collect();
        assert_eq!(parts, vec!["get", "HTTP", "Server", "v2"]);
        let parts: Vec<&str> = split_identifier("RetryReader")
            .into_iter()
            .map(|p| p.1)
            .collect();
        assert_eq!(parts, vec!["Retry", "Reader"]);
    }

    #[test]
    fn code_tokens_include_whole_identifier() {
        let mut tok = CodeTokenizer::default();
        let mut stream = tok.token_stream("fn u64_to_sql(x)");
        let mut texts = Vec::new();
        while stream.advance() {
            texts.push(stream.token().text.clone());
        }
        assert_eq!(texts, vec!["fn", "u64_to_sql", "u64", "to", "sql", "x"]);
    }
}
