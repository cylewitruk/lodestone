//! End-to-end tests against the `lode` binary with a deterministic embedder
//! (`LODESTONE_TEST_EMBEDDER=hash`): REST, MCP over streamable HTTP, watcher freshness, branch
//! switches, concurrent clients and crash consistency.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lodestone::api::mcp::{SEMANTIC_DESCRIPTION, SIMILAR_DESCRIPTION, STATUS_DESCRIPTION};
use lodestone::api::types::{SearchResponse, StatusReport};
use lodestone::config::Config;
use lodestone::index::{ItemStore, Lexical, Manifest, TokenizerKind, VectorIndex, worktree_id};
use lodestone::testutil::tempdir;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_lode");

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A Cargo workspace with `n` generated modules.
fn repo(n: usize) -> PathBuf {
    let root = tempdir().join("repo");
    write(&root, "Cargo.toml", "[workspace]\nmembers = [\"core\"]\n");
    write(&root, "core/Cargo.toml", "[package]\nname = \"core\"\n");
    write(&root, ".gitignore", "target/\n");
    for i in 0..n {
        write(
            &root,
            &format!("core/src/m{i}.rs"),
            &format!(
                "/// Module {i} helper that computes value {i}.\npub fn helper_{i}(x: u64) -> u64 {{ x + {i} }}\n\n/// Second helper {i}.\npub fn other_{i}() {{}}\n"
            ),
        );
    }
    root
}

struct Daemon {
    child: Child,
    base: String,
}

impl Daemon {
    fn start(home: &Path, delay_ms: u64) -> Self {
        let port = free_port();
        let child = Command::new(BIN)
            .args(["serve", "--port", &port.to_string()])
            .env("LODESTONE_HOME", home)
            .env("LODESTONE_TEST_EMBEDDER", "hash")
            .env("LODESTONE_TEST_EMBED_DELAY_MS", delay_ms.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon {
            child,
            base: format!("http://127.0.0.1:{port}"),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while ureq::get(format!("{}/v1/repos", d.base)).call().is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    fn post(&self, path: &str, body: Value) -> Value {
        ureq::post(format!("{}{path}", self.base))
            .send_json(&body)
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }

    fn status(&self) -> StatusReport {
        ureq::get(format!("{}/v1/status", self.base))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }

    fn search(&self, q: &str) -> SearchResponse {
        serde_json::from_value(self.post("/v1/search", json!({ "query": q, "top_k": 8 }))).unwrap()
    }

    fn wait_fresh(&self, timeout: Duration) -> Duration {
        let start = Instant::now();
        loop {
            let st = self.status();
            if st.files_indexed > 0 && !st.stale && !st.indexing.running {
                return start.elapsed();
            }
            assert!(
                start.elapsed() < timeout,
                "not fresh after {timeout:?}: {st:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal MCP client over streamable HTTP (JSON or SSE responses).
struct Mcp {
    url: String,
    session: Option<String>,
    next: u64,
}

impl Mcp {
    fn connect(base: &str) -> Self {
        let mut m = Mcp {
            url: format!("{base}/mcp"),
            session: None,
            next: 1,
        };
        m.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }));
        m.notify("notifications/initialized");
        m
    }

    fn send(&mut self, body: Value) -> Option<Value> {
        let mut req = ureq::post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", "2025-06-18");
        if let Some(s) = &self.session {
            req = req.header("Mcp-Session-Id", s);
        }
        let mut resp = req.send_json(&body).unwrap();
        if let Some(s) = resp.headers().get("mcp-session-id") {
            self.session = Some(s.to_str().unwrap().to_string());
        }
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let id = body.get("id").cloned()?;
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            return Some(v);
        }
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v.get("id") == Some(&id))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let v = self
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .expect("response");
        assert!(v.get("error").is_none(), "{v}");
        v["result"].clone()
    }

    fn notify(&mut self, method: &str) {
        self.send(json!({ "jsonrpc": "2.0", "method": method }));
    }
}

#[test]
fn daemon_rest_mcp_and_freshness() {
    let root = repo(20);
    let home = tempdir();
    let d = Daemon::start(&home, 0);
    d.post("/v1/register", json!({ "path": root.to_string_lossy() }));
    d.wait_fresh(Duration::from_secs(30));
    assert_eq!(d.search("helper_7").results[0].name, "helper_7");

    // MCP: three tools with the exact descriptions, all callable.
    let mut mcp = Mcp::connect(&d.base);
    let tools = mcp.request("tools/list", json!({}))["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(tools.len(), 3);
    let desc = |n: &str| {
        tools.iter().find(|t| t["name"] == n).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(desc("semantic_code_search"), SEMANTIC_DESCRIPTION);
    assert_eq!(desc("similar_code_search"), SIMILAR_DESCRIPTION);
    assert_eq!(desc("status"), STATUS_DESCRIPTION);
    let out = mcp.request(
        "tools/call",
        json!({ "name": "semantic_code_search", "arguments": { "query": "helper_3" } }),
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(
        text.lines().next().unwrap().starts_with(
            "core/src/m3.rs:2:8  lines 2-2  pub fn helper_3(x: u64) -> u64  — Module 3 helper"
        ),
        "{text}"
    );
    let out = mcp.request(
        "tools/call",
        json!({ "name": "similar_code_search", "arguments": { "code": "pub fn helper_3(x: u64) -> u64 { x + 3 }" } }),
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("fn helper_3("), "{text}");
    assert!(
        text.contains("# note: left out the passed code itself: core/src/m3.rs:2 helper_3"),
        "{text}"
    );
    let out = mcp.request("tools/call", json!({ "name": "status", "arguments": {} }));
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("files_indexed: 20")
    );

    // Edit → searchable within 2 s.
    let t = Instant::now();
    write(
        &root,
        "core/src/fresh.rs",
        "/// Freshly written.\npub fn freshly_written_helper() {}\n",
    );
    loop {
        if d.search("freshly_written_helper")
            .results
            .first()
            .is_some_and(|r| r.name == "freshly_written_helper")
        {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "edit not searchable within 2s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("edit_to_searchable_ms={}", t.elapsed().as_millis());

    // Deletion propagates.
    fs::remove_file(root.join("core/src/m0.rs")).unwrap();
    let t = Instant::now();
    while d
        .search("helper_0")
        .results
        .iter()
        .any(|r| r.name == "helper_0")
    {
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn branch_switch_rescans_with_cache_hits() {
    let root = repo(200);
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    git(&root, &["checkout", "-q", "-b", "feature"]);
    for i in 0..5 {
        write(
            &root,
            &format!("core/src/m{i}.rs"),
            &format!("/// Feature version {i}.\npub fn feature_{i}() {{}}\n"),
        );
    }
    git(&root, &["commit", "-q", "-am", "feature"]);
    git(&root, &["checkout", "-q", "main"]);
    let home = tempdir();
    let d = Daemon::start(&home, 0);
    d.post("/v1/register", json!({ "path": root.to_string_lossy() }));
    d.wait_fresh(Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(800));
    let t = Instant::now();
    git(&root, &["checkout", "-q", "feature"]);
    loop {
        let found = d
            .search("feature_3")
            .results
            .first()
            .is_some_and(|r| r.name == "feature_3");
        let st = d.status();
        if found && !st.stale {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "branch switch not fresh in 30s: {st:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("branch_switch_fresh_ms={}", t.elapsed().as_millis());
    // Switching back is all cache hits.
    git(&root, &["checkout", "-q", "main"]);
    let t = Instant::now();
    loop {
        let st = d.status();
        let back = d
            .search("helper_3")
            .results
            .first()
            .is_some_and(|r| r.name == "helper_3");
        if back
            && !st.stale
            && st
                .last_report
                .as_ref()
                .is_some_and(|r| r.files_changed >= 5)
        {
            let r = st.last_report.unwrap();
            let total = (r.chunks_cached + r.chunks_embedded).max(1);
            let hit = r.chunks_cached as f64 / total as f64;
            println!(
                "branch_switch_back_cache_hit_rate={hit:.3} files_changed={}",
                r.files_changed
            );
            assert!(hit >= 0.95, "{r:?}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn concurrent_clients_during_reindex() {
    let root = repo(300);
    let home = tempdir();
    let d = Daemon::start(&home, 20);
    d.post("/v1/register", json!({ "path": root.to_string_lossy() }));
    d.wait_fresh(Duration::from_secs(60));
    let stop = Arc::new(AtomicBool::new(false));
    let editor = {
        let (root, stop) = (root.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) {
                write(
                    &root,
                    &format!("core/src/m{}.rs", i % 300),
                    &format!("/// Edited {i}.\npub fn edited_{i}() {{}}\n"),
                );
                i += 1;
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let errors = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (base, errors, latencies) = (d.base.clone(), errors.clone(), latencies.clone());
            std::thread::spawn(move || {
                for i in 0..200 {
                    let start = Instant::now();
                    let r = ureq::post(format!("{base}/v1/search")).send_json(
                        json!({ "query": format!("helper {} value", (t * 200 + i) % 300) }),
                    );
                    match r {
                        Ok(mut resp) if resp.status() == 200 => {
                            let _: SearchResponse = resp.body_mut().read_json().unwrap();
                        }
                        _ => {
                            errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    latencies
                        .lock()
                        .unwrap()
                        .push(start.elapsed().as_secs_f64() * 1000.0);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    editor.join().unwrap();
    let mut l = latencies.lock().unwrap().clone();
    l.sort_by(f64::total_cmp);
    println!(
        "concurrent: requests={} errors={} p50_ms={:.1} p99_ms={:.1}",
        l.len(),
        errors.load(Ordering::SeqCst),
        l[l.len() / 2],
        l[l.len() * 99 / 100]
    );
    assert_eq!(errors.load(Ordering::SeqCst), 0);
    assert_eq!(l.len(), 1600);
}

/// Kill an indexing process mid-run, then restart: the index must be consistent (every
/// manifest entry backed by items, every item by a vector and a lexical doc).
#[test]
fn kill_mid_index_leaves_consistent_index() {
    let root = repo(600);
    let home = tempdir();
    let mut child = Command::new(BIN)
        .args(["--local", "index", &root.to_string_lossy()])
        .env("LODESTONE_HOME", &home)
        .env("LODESTONE_TEST_EMBEDDER", "hash")
        .env("LODESTONE_TEST_EMBED_DELAY_MS", "150")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let canonical = fs::canonicalize(&root).unwrap();
    let dir = home.join("worktrees").join(worktree_id(&canonical));
    let manifest_path = dir.join("manifest.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let n = Manifest::load(&manifest_path)
            .ok()
            .flatten()
            .map(|m| m.files.len())
            .unwrap_or(0);
        if n > 0 {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(120));
    child.kill().unwrap();
    child.wait().unwrap();
    let partial = Manifest::load(&manifest_path).unwrap().unwrap();
    assert!(
        partial.files.len() < 600,
        "killed too late: {}",
        partial.files.len()
    );

    let ok = Command::new(BIN)
        .args(["--local", "index", &root.to_string_lossy()])
        .env("LODESTONE_HOME", &home)
        .env("LODESTONE_TEST_EMBEDDER", "hash")
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let m = Manifest::load(&manifest_path).unwrap().unwrap();
    assert_eq!(m.files.len(), 600);
    let rows = ItemStore::open(&dir.join("items.sqlite"))
        .unwrap()
        .load_all()
        .unwrap();
    let ids: std::collections::HashSet<u64> = rows.iter().map(|r| r.id).collect();
    let manifest_ids: std::collections::HashSet<u64> = m
        .files
        .values()
        .flat_map(|e| e.chunk_ids.iter().copied())
        .collect();
    assert_eq!(ids, manifest_ids, "items and manifest disagree");
    let config = Config {
        data_dir: Some(home.clone()),
        ..Config::default()
    };
    let v = VectorIndex::load(&dir.join("vectors.usearch"), 64, config.vector_precision)
        .unwrap()
        .unwrap();
    assert!(
        rows.iter().all(|r| v.contains(r.id)),
        "items without vectors"
    );
    let lex = Lexical::open(&dir.join("lexical"), TokenizerKind::Code).unwrap();
    assert_eq!(lex.searcher().num_docs() as usize, rows.len());
    println!(
        "kill_mid_index: partial_files={} final_items={}",
        partial.files.len(),
        rows.len()
    );
}
