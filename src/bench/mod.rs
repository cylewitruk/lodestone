//! `lode bench`: retrieval quality (rank@k, MRR, negative-score gap), index time and query
//! latency against a generic question file `[{id, kind, question, answers:[{path, symbol}]}]`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::ensure;
use serde::{Deserialize, Serialize};

use crate::api::types::{DEFAULT_TOP_K, SearchRequest};
use crate::index::{IndexOptions, IndexReport};
use crate::registry::Engine;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Answer {
    #[serde(default)]
    pub path: Option<String>,
    pub symbol: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub id: String,
    pub kind: String,
    pub question: String,
    #[serde(default)]
    pub answers: Vec<Answer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct QuestionResult {
    pub id: String,
    pub kind: String,
    /// 1-based rank of the first correct answer within the top 50.
    pub rank: Option<usize>,
    pub top_similarity: Option<f32>,
    pub top: Vec<String>,
    pub latency_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct BenchReport {
    pub model: String,
    pub backend: String,
    pub cold_index: Option<IndexReport>,
    pub noop_index_secs: f64,
    pub questions: Vec<QuestionResult>,
    /// Results an agent sees per search by default ([`DEFAULT_TOP_K`]).
    pub agent_top_k: usize,
    /// Share of answerable questions whose answer is within `agent_top_k`.
    pub rank_at_agent_k: f64,
    /// Questions with at least one expected answer (the denominator of every rank metric).
    pub answerable: usize,
    pub rank_at_1: f64,
    pub rank_at_5: f64,
    pub rank_at_10: f64,
    /// Answer anywhere in the 50 results: the candidate pool a reranker reorders.
    pub rank_at_50: f64,
    pub mrr: f64,
    pub rank_at_5_by_kind: Vec<(String, f64)>,
    pub hard_hits_top5: Option<(usize, usize)>,
    pub hard_hits_agent_k: Option<(usize, usize)>,
    pub negative_max_similarity: Option<f32>,
    pub positive_min_similarity: Option<f32>,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    /// Fraction of answer symbols that rank first when searched for verbatim.
    pub identifier_hit_rate: f64,
}

pub struct BenchOptions {
    pub corpus: PathBuf,
    pub questions: PathBuf,
    pub hard: Vec<String>,
    pub skip_index: bool,
    pub force: bool,
    /// Search with `include_tests` (test items down-weighted instead of filtered).
    pub include_tests: bool,
}

fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[((s.len() - 1) as f64 * p).round() as usize]
}

fn matches(path: &str, name: &str, a: &Answer) -> bool {
    a.path.as_deref().is_none_or(|p| p == path) && a.symbol == name
}

pub fn run(engine: &Engine, opts: &BenchOptions) -> anyhow::Result<BenchReport> {
    let questions: Vec<Question> = serde_json::from_slice(&std::fs::read(&opts.questions)?)?;
    let w = engine.register(&opts.corpus)?;
    let mut report = BenchReport {
        model: engine.model_id().to_string(),
        backend: engine.config.backend.to_string(),
        ..BenchReport::default()
    };
    if !opts.skip_index {
        let r = engine.index_blocking(
            &w,
            IndexOptions {
                force: opts.force,
                paths: None,
            },
        )?;
        eprintln!(
            "index: {} files changed, {} items, {} embedded, {} cached in {:.1}s",
            r.files_changed, r.items, r.chunks_embedded, r.chunks_cached, r.secs
        );
        report.cold_index = Some(r);
        let t = Instant::now();
        engine.index_blocking(&w, IndexOptions::default())?;
        report.noop_index_secs = t.elapsed().as_secs_f64();
    }
    let repo = Some(w.root.to_string_lossy().to_string());
    let req = |q: &Question| SearchRequest {
        query: q.question.clone(),
        repo: repo.clone(),
        top_k: 50,
        include_tests: opts.include_tests,
        ..SearchRequest::default()
    };
    // Warm-up pass loads the model and pages in the index. A degraded search (stale index,
    // reranker or embedder unavailable) would still produce plausible numbers, so it fails here.
    for q in &questions {
        let resp = engine.search(&req(q))?;
        ensure!(
            resp.notes.is_empty() && !resp.stale,
            "degraded search for {}: notes {:?}, stale {:?}",
            q.id,
            resp.notes,
            resp.stale_reasons
        );
    }
    let mut latencies = Vec::new();
    for q in &questions {
        let t = Instant::now();
        let resp = engine.search(&req(q))?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        latencies.push(ms);
        let rank = resp
            .results
            .iter()
            .position(|r| q.answers.iter().any(|a| matches(&r.path, &r.name, a)))
            .map(|i| i + 1);
        report.questions.push(QuestionResult {
            id: q.id.clone(),
            kind: q.kind.clone(),
            rank,
            top_similarity: resp
                .results
                .iter()
                .filter_map(|r| r.similarity)
                .reduce(f32::max),
            top: resp
                .results
                .iter()
                .take(5)
                .map(|r| format!("{}::{}", r.path, r.name))
                .collect(),
            latency_ms: ms,
        });
    }
    let positives: Vec<&QuestionResult> = report
        .questions
        .iter()
        .filter(|q| {
            !questions
                .iter()
                .any(|x| x.id == q.id && x.answers.is_empty())
        })
        .collect();
    let n = positives.len().max(1) as f64;
    let at = |k: usize, qs: &[&QuestionResult]| {
        qs.iter().filter(|q| q.rank.is_some_and(|r| r <= k)).count() as f64 / qs.len().max(1) as f64
    };
    report.answerable = positives.len();
    report.agent_top_k = DEFAULT_TOP_K;
    report.rank_at_agent_k = at(DEFAULT_TOP_K, &positives);
    report.rank_at_1 = at(1, &positives);
    report.rank_at_5 = at(5, &positives);
    report.rank_at_10 = at(10, &positives);
    report.rank_at_50 = at(50, &positives);
    report.mrr = positives
        .iter()
        .map(|q| q.rank.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / n;
    let mut kinds: Vec<String> = positives.iter().map(|q| q.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    report.rank_at_5_by_kind = kinds
        .into_iter()
        .map(|k| {
            let qs: Vec<&QuestionResult> =
                positives.iter().filter(|q| q.kind == k).copied().collect();
            let v = at(5, &qs);
            (k, v)
        })
        .collect();
    // Hard ids not in this question file are ignored rather than counted as misses.
    let hard: Vec<&QuestionResult> = report
        .questions
        .iter()
        .filter(|q| opts.hard.contains(&q.id))
        .collect();
    if !hard.is_empty() {
        let hits = |k: usize| {
            hard.iter()
                .filter(|q| q.rank.is_some_and(|r| r <= k))
                .count()
        };
        report.hard_hits_top5 = Some((hits(5), hard.len()));
        report.hard_hits_agent_k = Some((hits(DEFAULT_TOP_K), hard.len()));
    }
    report.negative_max_similarity = report
        .questions
        .iter()
        .filter(|q| {
            questions
                .iter()
                .any(|x| x.id == q.id && x.answers.is_empty())
        })
        .filter_map(|q| q.top_similarity)
        .reduce(f32::max);
    report.positive_min_similarity = positives
        .iter()
        .filter_map(|q| q.top_similarity)
        .reduce(f32::min);
    let symbols: Vec<&str> = {
        let mut v: Vec<&str> = questions
            .iter()
            .flat_map(|q| q.answers.iter().map(|a| a.symbol.as_str()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let mut ident_hits = 0;
    for s in &symbols {
        let resp = engine.search(&SearchRequest {
            query: s.to_string(),
            repo: repo.clone(),
            top_k: 1,
            include_tests: true,
            ..SearchRequest::default()
        })?;
        ident_hits += resp.results.first().is_some_and(|r| r.name == *s) as usize;
    }
    report.identifier_hit_rate = ident_hits as f64 / symbols.len().max(1) as f64;
    report.latency_p50_ms = percentile(&latencies, 0.5);
    report.latency_p95_ms = percentile(&latencies, 0.95);
    Ok(report)
}

impl BenchReport {
    pub fn to_text(&self) -> String {
        let mut l = vec![format!("model: {} ({})", self.model, self.backend)];
        if let Some(r) = &self.cold_index {
            l.push(format!(
                "index: {:.1}s ({} files, {} items, {} embedded, {} cached); no-op re-index {:.2}s",
                r.secs,
                r.files_changed,
                r.items,
                r.chunks_embedded,
                r.chunks_cached,
                self.noop_index_secs
            ));
        }
        for q in &self.questions {
            l.push(format!(
                "{} [{}] rank={} sim={} top={}",
                q.id,
                q.kind,
                q.rank.map_or("-".to_string(), |r| r.to_string()),
                q.top_similarity
                    .map_or("-".to_string(), |s| format!("{s:.3}")),
                q.top.first().cloned().unwrap_or_default()
            ));
        }
        l.push(format!(
            "agent view: answer within the default top {}: {:.3} ({}/{})",
            self.agent_top_k,
            self.rank_at_agent_k,
            (self.rank_at_agent_k * self.answerable as f64).round() as usize,
            self.answerable
        ));
        l.push(format!(
            "rank@1 {:.3}  rank@5 {:.3}  rank@10 {:.3}  MRR {:.3}  candidate recall@50 {:.3}",
            self.rank_at_1, self.rank_at_5, self.rank_at_10, self.mrr, self.rank_at_50
        ));
        for (k, v) in &self.rank_at_5_by_kind {
            l.push(format!("rank@5[{k}] {v:.3}"));
        }
        if let (Some((h5, n)), Some((hk, _))) = (self.hard_hits_top5, self.hard_hits_agent_k) {
            l.push(format!(
                "hard cases in top 5: {h5}/{n}; in top {}: {hk}/{n}",
                self.agent_top_k
            ));
        }
        if let (Some(neg), Some(pos)) = (self.negative_max_similarity, self.positive_min_similarity)
        {
            l.push(format!("negative max top-similarity {neg:.3}; positive min top-similarity {pos:.3}; gap {:.3}", pos - neg));
        }
        l.push(format!(
            "identifier queries: top-1 hit rate {:.3}",
            self.identifier_hit_rate
        ));
        l.push(format!(
            "query latency p50 {:.1} ms  p95 {:.1} ms",
            self.latency_p50_ms, self.latency_p95_ms
        ));
        l.join("\n")
    }
}

/// Resolves `<eval-dir>` defaults: `questions.json` and `corpus/` inside it.
pub fn default_paths(eval_dir: &Path) -> (PathBuf, PathBuf) {
    (eval_dir.join("corpus"), eval_dir.join("questions.json"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::Config;
    use crate::embed::HashEmbedder;
    use crate::testutil::tempdir;

    #[test]
    fn agent_view_and_hard_cases() {
        let corpus = tempdir().join("repo");
        std::fs::create_dir_all(corpus.join("src")).unwrap();
        std::fs::write(
            corpus.join("src/lib.rs"),
            "/// Copy a directory tree recursively.\npub fn copy_dir_all() {}\n\n/// Parse arguments.\npub fn parse_args() {}\n",
        )
        .unwrap();
        let questions = corpus.join("questions.json");
        std::fs::write(
            &questions,
            r#"[
              {"id": "q1", "kind": "easy", "question": "copy a directory tree recursively",
               "answers": [{"symbol": "copy_dir_all"}]},
              {"id": "q2", "kind": "mismatch", "question": "something absent",
               "answers": [{"symbol": "not_in_repo"}]},
              {"id": "q3", "kind": "negative", "question": "nothing like this", "answers": []}
            ]"#,
        )
        .unwrap();
        let mut config = Config {
            data_dir: Some(tempdir()),
            ..Config::default()
        };
        config.rerank.enabled = false;
        let emb = Arc::new(HashEmbedder {
            dim: 64,
            id: "test/hash".into(),
            delay_ms: 0,
        });
        let engine = Engine::with_embedder(config, emb).unwrap();
        let report = run(
            &engine,
            &BenchOptions {
                corpus,
                questions,
                hard: vec!["q1".into(), "unknown".into()],
                skip_index: false,
                force: false,
                include_tests: false,
            },
        )
        .unwrap();
        assert_eq!(report.answerable, 2);
        assert_eq!(report.agent_top_k, DEFAULT_TOP_K);
        assert_eq!(report.rank_at_agent_k, 0.5);
        assert_eq!(report.rank_at_50, 0.5);
        assert_eq!(report.hard_hits_top5, Some((1, 1)));
        assert_eq!(report.hard_hits_agent_k, Some((1, 1)));
        assert!(
            report
                .to_text()
                .contains("agent view: answer within the default top 8: 0.500 (1/2)")
        );
    }
}
