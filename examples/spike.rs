//! Phase 0 spike: chunk a corpus, then embed the first N chunks with one backend × model and
//! report chunks/s. Run under `/usr/bin/time -l` for peak RSS.
//!
//! `cargo run --release --example spike -- <corpus> <backend|chunks> <model> [n] [names,...]`

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use lodestone::chunk::{Chunker, LanguageRegistry};
use lodestone::config::Config;
use lodestone::embed::{self, InputKind};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let corpus = Path::new(&args[1]);
    let mode = args[2].as_str();
    let model = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "jina-code-v2".into());
    let n: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let names: Vec<String> = args
        .get(5)
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();
    let cfg = Config::load(None)?;
    let reg = Arc::new(LanguageRegistry::load(&cfg.languages, &cfg.data_dir()));
    let chunker = Chunker::new(reg.clone(), cfg.chunk_tokens);
    let mut files: Vec<_> = ignore::WalkBuilder::new(corpus)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && reg.accepts(e.path()))
        .map(|e| e.into_path())
        .collect();
    files.sort();
    let t = Instant::now();
    let mut items = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(corpus)?.to_string_lossy().to_string();
        items.extend(chunker.chunk(&rel, &std::fs::read(f)?));
    }
    let tagged = items.iter().filter(|i| i.tagged).count();
    println!(
        "files={} items={} tagged={} text={} chunk_secs={:.2} avg_chars={}",
        files.len(),
        items.len(),
        tagged,
        items.len() - tagged,
        t.elapsed().as_secs_f64(),
        items.iter().map(|i| i.text.len()).sum::<usize>() / items.len().max(1)
    );
    for name in &names {
        for i in items.iter().filter(|i| &i.name == name) {
            println!(
                "{}:{} [{}] {} | {} | test={}",
                i.rel_path,
                i.line_start,
                i.kind,
                i.signature,
                i.doc_first_line(),
                i.is_test
            );
        }
    }
    if mode == "chunks" {
        return Ok(());
    }
    let backend: lodestone::config::Backend = mode.parse()?;
    let t = Instant::now();
    let embedder = embed::load(&cfg, &model, backend)?;
    let load = t.elapsed().as_secs_f64();
    // Deterministic sample spread across the corpus.
    let step = (items.len() / n).max(1);
    let sample: Vec<&str> = items
        .iter()
        .step_by(step)
        .take(n)
        .map(|i| i.text.as_str())
        .collect();
    let warm = embedder.embed(&sample[..sample.len().min(64)], InputKind::Document)?;
    let t = Instant::now();
    let mut vecs = Vec::with_capacity(sample.len());
    for chunk in sample.chunks(1024) {
        vecs.extend(embedder.embed(chunk, InputKind::Document)?);
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "backend={backend} model={model} n={} load_secs={load:.1} embed_secs={secs:.1} chunks_per_s={:.1} dim={}",
        sample.len(),
        sample.len() as f64 / secs,
        warm[0].len()
    );
    let q = embedder.embed(&["query latency probe"], InputKind::Query)?;
    let t = Instant::now();
    for _ in 0..20 {
        embedder.embed(
            &["retry a flaky http call with exponential backoff"],
            InputKind::Query,
        )?;
    }
    println!(
        "query_embed_ms={:.1} dim={}",
        t.elapsed().as_secs_f64() * 1000.0 / 20.0,
        q[0].len()
    );
    if let Ok(path) = std::env::var("SPIKE_DUMP") {
        let first: Vec<&Vec<f32>> = vecs.iter().take(200).collect();
        std::fs::write(path, serde_json::to_string(&first)?)?;
    }
    Ok(())
}
