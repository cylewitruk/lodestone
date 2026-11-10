//! Times the candle cross-encoder on N synthetic (query, document) pairs.
//! `cargo run --release --example rerank_bench -- <hf_repo> <max_tokens> <n> [sub_batch]`

use std::time::Instant;

use lodestone::config::{Backend, Config};
use lodestone::search::rerank::Reranker;
use lodestone::search::rerank::candle_impl::CandleReranker;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let cfg = Config::load(None)?;
    let r = CandleReranker::load(
        &a[1],
        Backend::CandleMetal,
        &cfg.data_dir().join("models"),
        a[2].parse()?,
    )?;
    let n: usize = a[3].parse()?;
    let sub: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(n);
    let doc = "pub fn structured_data_message_hash(structured_data: Value, domain: Value) -> Sha256Sum { let message = [STRUCTURED_DATA_PREFIX.as_ref(), structured_data_hash(domain).as_bytes()].concat(); Sha256Sum::from_data(&message) } ".repeat(6);
    let docs: Vec<&str> = (0..n).map(|_| doc.as_str()).collect();
    let q = "I need the digest a Stacks wallet signs for off-chain typed data: a fixed prefix, then the hash of a domain tuple, then the hash of the message.";
    r.score(q, &docs[..sub.min(n)])?;
    for _ in 0..3 {
        let t = Instant::now();
        for c in docs.chunks(sub) {
            r.score(q, c)?;
        }
        println!(
            "n={n} sub_batch={sub} ms={:.1}",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}
