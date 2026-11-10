//! Cross-backend agreement: embeds the same corpus chunks with two backends and reports the
//! cosine similarity between their vectors (≈1.0 means the candle port matches the reference).
//!
//! `cargo run --release --example agree -- <corpus> <model> <backend-a> <backend-b> [n]`

use std::path::Path;
use std::sync::Arc;

use lodestone::chunk::{Chunker, LanguageRegistry};
use lodestone::config::Config;
use lodestone::embed::{self, InputKind};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let corpus = Path::new(&args[1]);
    let model = &args[2];
    let (a, b) = (args[3].parse()?, args[4].parse()?);
    let n: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(200);
    let cfg = Config::load(None)?;
    let reg = Arc::new(LanguageRegistry::load(&cfg.languages, &cfg.data_dir()));
    let chunker = Chunker::new(reg.clone(), cfg.chunk_tokens);
    let (files, _) = lodestone::index::walk(corpus, &|p: &Path| reg.accepts(p));
    let mut texts = Vec::new();
    for f in files.iter().step_by(7) {
        for i in chunker
            .chunk(f, &std::fs::read(corpus.join(f))?)
            .into_iter()
            .take(1)
        {
            texts.push(i.text);
        }
        if texts.len() >= n {
            break;
        }
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let va = embed::load(&cfg, model, a)?.embed(&refs, InputKind::Document)?;
    let vb = embed::load(&cfg, model, b)?.embed(&refs, InputKind::Document)?;
    let sims: Vec<f32> = va
        .iter()
        .zip(&vb)
        .map(|(x, y)| embed::cosine(x, y))
        .collect();
    let mean = sims.iter().sum::<f32>() / sims.len() as f32;
    let min = sims.iter().cloned().fold(f32::MAX, f32::min);
    println!(
        "model={model} {a} vs {b}: n={} mean_cos={mean:.4} min_cos={min:.4}",
        sims.len()
    );
    Ok(())
}
