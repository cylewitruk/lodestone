//! Sanity check for a model before a full benchmark: each query must score its own document
//! highest, and all scores must be finite.
//! `cargo run --release --example model_smoke -- embed <model-alias>`
//! `cargo run --release --example model_smoke -- rerank <hf_repo>`

use std::time::Instant;

use anyhow::{bail, ensure};
use lodestone::config::{Backend, Config};
use lodestone::embed::{self, InputKind, cosine};
use lodestone::search::rerank::Reranker;
use lodestone::search::rerank::candle_impl::CandleReranker;

const PAIRS: [(&str, &str); 4] = [
    (
        "recursively copy a directory tree to a new location",
        "src/fs.rs function copy_dir_all\npub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> { fs::create_dir_all(dst)?; for entry in fs::read_dir(src)? { let entry = entry?; if entry.file_type()?.is_dir() { copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?; } else { fs::copy(entry.path(), dst.join(entry.file_name()))?; } } Ok(()) }",
    ),
    (
        "retry a flaky call with exponentially growing delays",
        "src/net.rs function retry_with_backoff\npub fn retry_with_backoff<T>(mut f: impl FnMut() -> Result<T>, deadline: Duration) -> Result<T> { let mut delay = Duration::from_millis(100); loop { match f() { Ok(v) => return Ok(v), Err(e) if start.elapsed() > deadline => return Err(e), Err(_) => { sleep(delay); delay *= 2; } } } }",
    ),
    (
        "current unix time in whole seconds",
        "src/time.rs function now_secs\npub fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }",
    ),
    (
        "merge overlapping ranges into a sorted disjoint list",
        "src/ranges.rs function merge_ranges\nfn merge_ranges(mut v: Vec<Range<usize>>) -> Vec<Range<usize>> { v.sort_by_key(|r| r.start); let mut out: Vec<Range<usize>> = Vec::new(); for r in v { match out.last_mut() { Some(last) if r.start <= last.end => last.end = last.end.max(r.end), _ => out.push(r) } } out }",
    ),
];

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(target)) = (a.get(1), a.get(2)) else {
        bail!("usage: model_smoke embed <alias> | rerank <hf_repo>");
    };
    let cfg = Config::load(None)?;
    let queries: Vec<&str> = PAIRS.iter().map(|p| p.0).collect();
    let docs: Vec<&str> = PAIRS.iter().map(|p| p.1).collect();
    let t = Instant::now();
    let scores: Vec<Vec<f32>> = match mode.as_str() {
        "embed" => {
            let e = embed::load(&cfg, target, Backend::CandleMetal)?;
            let q = e.embed(&queries, InputKind::Query)?;
            let d = e.embed(&docs, InputKind::Document)?;
            q.iter()
                .map(|qv| d.iter().map(|dv| cosine(qv, dv)).collect())
                .collect()
        }
        "rerank" => {
            let r = CandleReranker::load(
                target,
                Backend::CandleMetal,
                &cfg.data_dir().join("models"),
                192,
            )?;
            queries
                .iter()
                .map(|q| r.score(q, &docs))
                .collect::<anyhow::Result<_>>()?
        }
        other => bail!("unknown mode {other}"),
    };
    for (i, row) in scores.iter().enumerate() {
        let best = row
            .iter()
            .enumerate()
            .max_by(|x, y| x.1.total_cmp(y.1))
            .map(|(j, _)| j);
        println!("q{i}: {row:.3?} best={best:?}");
        ensure!(row.iter().all(|s| s.is_finite()), "non-finite score");
        ensure!(
            best == Some(i),
            "query {i} did not rank its own document first"
        );
    }
    println!("ok in {:.1}s (includes load)", t.elapsed().as_secs_f64());
    Ok(())
}
