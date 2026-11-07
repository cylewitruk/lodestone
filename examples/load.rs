//! Phase 2 load test against a running daemon: N clients × M searches (queries from a question
//! file) while an editor thread keeps modifying files in the worktree so watcher-triggered
//! reindexing runs underneath. Prints errors, p50 and p99.
//!
//! `cargo run --release --example load -- <base-url> <questions.json> <worktree> [clients] [per-client]`

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let base = args[1].clone();
    let questions: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[2])?)?;
    let worktree = PathBuf::from(&args[3]);
    let clients: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let per: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(200);
    let queries: Arc<Vec<String>> = Arc::new(
        questions
            .iter()
            .filter_map(|q| q["question"].as_str().map(String::from))
            .collect(),
    );
    let files: Vec<PathBuf> = ignore::WalkBuilder::new(&worktree)
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .take(400)
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let edits = Arc::new(AtomicUsize::new(0));
    let editor = {
        let (stop, edits) = (stop.clone(), edits.clone());
        std::thread::spawn(move || {
            let mut originals: Vec<(PathBuf, Vec<u8>)> = Vec::new();
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let f: &Path = &files[(i * 7) % files.len()];
                let orig = std::fs::read(f).unwrap();
                if !originals.iter().any(|(p, _)| p == f) {
                    originals.push((f.to_path_buf(), orig.clone()));
                }
                let mut changed = orig.clone();
                changed.extend_from_slice(
                    format!("\n/// load-test edit {i}\npub fn load_test_edit_{i}() {{}}\n")
                        .as_bytes(),
                );
                std::fs::write(f, changed).unwrap();
                edits.fetch_add(1, Ordering::SeqCst);
                i += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            for (p, b) in originals {
                std::fs::write(p, b).unwrap();
            }
        })
    };
    std::thread::sleep(Duration::from_millis(600));
    let errors = Arc::new(AtomicUsize::new(0));
    let stale_seen = Arc::new(AtomicUsize::new(0));
    let lat = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|c| {
            let (base, queries, errors, lat, stale_seen) = (
                base.clone(),
                queries.clone(),
                errors.clone(),
                lat.clone(),
                stale_seen.clone(),
            );
            std::thread::spawn(move || {
                for i in 0..per {
                    let q = &queries[(c * per + i) % queries.len()];
                    let t = Instant::now();
                    match ureq::post(format!("{base}/v1/search"))
                        .send_json(json!({ "query": q, "top_k": 8 }))
                    {
                        Ok(mut r) => {
                            let v: Value = r.body_mut().read_json().unwrap_or(Value::Null);
                            if v["results"].as_array().is_none_or(|a| a.is_empty()) {
                                errors.fetch_add(1, Ordering::SeqCst);
                            }
                            if v["stale"].as_bool() == Some(true) {
                                stale_seen.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    lat.lock().unwrap().push(t.elapsed().as_secs_f64() * 1000.0);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let wall = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::SeqCst);
    editor.join().unwrap();
    let mut l = lat.lock().unwrap().clone();
    l.sort_by(f64::total_cmp);
    println!(
        "clients={clients} per_client={per} requests={} errors={} stale_responses={} edits={} wall_s={wall:.1} p50_ms={:.1} p99_ms={:.1} max_ms={:.1}",
        l.len(),
        errors.load(Ordering::SeqCst),
        stale_seen.load(Ordering::SeqCst),
        edits.load(Ordering::SeqCst),
        l[l.len() / 2],
        l[l.len() * 99 / 100],
        l[l.len() - 1]
    );
    Ok(())
}
