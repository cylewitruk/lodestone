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
use lodestone::api::types::{RegisterRequest, SearchResponse, SessionRequest, StatusReport};
use lodestone::client::Client;
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
        Self::start_with_env(home, delay_ms, &[])
    }

    fn start_with_env(home: &Path, delay_ms: u64, env: &[(&str, &Path)]) -> Self {
        let port = free_port();
        let child = Command::new(BIN)
            .args(["serve", "--port", &port.to_string()])
            .env("LODESTONE_HOME", home)
            .env("LODESTONE_TEST_EMBEDDER", "hash")
            .env("LODESTONE_TEST_EMBED_DELAY_MS", delay_ms.to_string())
            .envs(env.iter().copied())
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

#[test]
fn watcher_indexes_only_files_a_full_scan_would() {
    let root = repo(2);
    let home = tempdir();
    // The global excludes file, set explicitly so the machine's git config cannot interfere.
    write(&home, "global-ignore", "scratch/\n");
    write(
        &home,
        "gitconfig",
        &format!(
            "[core]\n\texcludesFile = {}\n",
            home.join("global-ignore").display()
        ),
    );
    let d = Daemon::start_with_env(&home, 0, &[("GIT_CONFIG_GLOBAL", &home.join("gitconfig"))]);
    d.post("/v1/register", json!({ "path": root.to_string_lossy() }));
    d.wait_fresh(Duration::from_secs(30));
    let before = d.status().files_indexed;

    // Excluded by a nested .gitignore, by being hidden, and by the global excludes file; only
    // the last file is indexable.
    write(&root, "core/nested/.gitignore", "ignored/\n");
    write(
        &root,
        "core/nested/ignored/n.rs",
        "pub fn nested_ignored() {}\n",
    );
    write(&root, "core/.hidden/h.rs", "pub fn in_hidden_dir() {}\n");
    write(&root, "scratch/g.rs", "pub fn globally_ignored() {}\n");
    write(&root, "core/src/fresh.rs", "pub fn freshly_added() {}\n");

    wait_until(
        "the new file to be indexed",
        Duration::from_secs(30),
        || d.status().files_indexed > before,
    );
    // Let any later batch of events land before checking nothing else came in.
    std::thread::sleep(Duration::from_secs(2));
    d.wait_fresh(Duration::from_secs(30));
    assert_eq!(d.status().files_indexed, before + 1);
    let paths: Vec<String> = [
        "nested_ignored",
        "in_hidden_dir",
        "globally_ignored",
        "freshly_added",
    ]
    .iter()
    .flat_map(|q| d.search(q).results)
    .map(|r| r.path)
    .collect();
    assert!(paths.iter().any(|p| p == "core/src/fresh.rs"), "{paths:?}");
    for excluded in [
        "core/nested/ignored/n.rs",
        "core/.hidden/h.rs",
        "scratch/g.rs",
    ] {
        assert!(
            !paths.iter().any(|p| p == excluded),
            "{excluded} indexed: {paths:?}"
        );
    }
}

/// A home whose config sets a short idle timeout for on-demand daemons, which listen only on the
/// home's Unix socket.
fn on_demand_home(idle_secs: u64) -> PathBuf {
    let home = tempdir();
    write(
        &home,
        "config.toml",
        &format!("idle_timeout_secs = {idle_secs}\n"),
    );
    home
}

fn lode(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("LODESTONE_HOME", home)
        .env("LODESTONE_TEST_EMBEDDER", "hash")
        .env_remove("LODESTONE_PORT")
        .env_remove("LODESTONE_SOCKET");
    c
}

fn home_config(home: &Path) -> Config {
    Config {
        data_dir: Some(home.to_path_buf()),
        ..Config::default()
    }
}

/// A client for the daemon serving `home`, if one answers.
fn client(home: &Path) -> Option<Client> {
    Client::connect(&home_config(home).endpoint())
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn on_demand_daemon_watches_leased_worktrees_and_exits_when_idle() {
    let root = repo(3);
    let home = on_demand_home(4);
    let mut child = lode(&home)
        .args(["serve", "--on-demand"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the daemon", Duration::from_secs(30), || {
        client(&home).is_some()
    });
    let c = client(&home).unwrap();
    let watcher = || c.status(None).unwrap().watcher;
    c.register(&RegisterRequest {
        path: root.to_string_lossy().into_owned(),
    })
    .unwrap();

    // A session in a subdirectory leases the worktree; it stays watched while the lease lives.
    let resp = c
        .session(&SessionRequest {
            id: "s1".into(),
            cwd: root.join("core").to_string_lossy().into_owned(),
            ttl_secs: 60,
        })
        .unwrap();
    assert_eq!(resp.repo.unwrap().name, "repo");
    assert!(watcher().starts_with("watching"));

    // Released (and the registration's lease, capped at the idle timeout, expired): unwatched,
    // then the daemon exits and removes its socket.
    c.release("s1").unwrap();
    wait_until("unwatch", Duration::from_secs(15), || {
        watcher().starts_with("not watched")
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not exit when idle");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!home_config(&home).socket_path().exists());
}

#[test]
fn one_daemon_per_socket_and_stale_sockets_are_replaced() {
    use std::os::unix::fs::PermissionsExt;

    let home = on_demand_home(0);
    // A socket file left by a daemon that crashed: nothing listens on it.
    let socket = home_config(&home).socket_path();
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists() && client(&home).is_none());

    let mut first = lode(&home)
        .args(["serve", "--on-demand"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the daemon", Duration::from_secs(30), || {
        client(&home).is_some()
    });
    let mode = fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");

    let second = lode(&home).arg("serve").output().unwrap();
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("another lodestone daemon"), "{err}");
    assert!(client(&home).is_some(), "the first daemon still answers");

    first.kill().unwrap();
    first.wait().unwrap();
}

#[test]
fn cli_starts_the_daemon_on_demand_but_status_does_not() {
    let root = repo(3);
    let home = on_demand_home(3);
    let run = |args: &[&str]| {
        let out = lode(&home).args(args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    };

    run(&["status"]);
    assert!(client(&home).is_none(), "status started a daemon");

    let (ok, text) = run(&["register", &root.to_string_lossy()]);
    assert!(ok, "{text}");
    assert!(text.contains("indexing in the daemon"), "{text}");
    assert!(client(&home).is_some());

    wait_until("a search to find helper_1", Duration::from_secs(30), || {
        let (ok, text) = run(&["search", "helper_1", "--repo", &root.to_string_lossy()]);
        ok && text.contains("pub fn helper_1")
    });

    // The CLI's leases are capped at the idle timeout, so the daemon exits soon after.
    wait_until("the idle daemon to exit", Duration::from_secs(30), || {
        client(&home).is_none()
    });
}

#[test]
fn cli_fails_fast_when_the_daemon_cannot_start() {
    let home = on_demand_home(3);
    // The socket's directory cannot be created, so the daemon exits at once.
    write(&home, "blocker", "");
    let socket = home.join("blocker").join("daemon.sock");
    write(
        &home,
        "config.toml",
        &format!(
            "idle_timeout_secs = 3\nsocket = {:?}\n",
            socket.to_string_lossy()
        ),
    );
    let started = Instant::now();
    let out = lode(&home).args(["search", "anything"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("daemon exited"), "{err}");
    assert!(err.contains("daemon.log"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn stop_and_sigterm_shut_the_daemon_down_gracefully() {
    let home = on_demand_home(0);
    let socket = home_config(&home).socket_path();
    let serve = |delay_ms: &str| {
        lode(&home)
            .args(["serve", "--on-demand"])
            .env("LODESTONE_TEST_EMBED_DELAY_MS", delay_ms)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let stop = || {
        let out = lode(&home).arg("stop").output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let mut daemon = serve("0");
    wait_until("the daemon", Duration::from_secs(30), || {
        client(&home).is_some()
    });
    assert!(stop().contains("stopped"));
    assert!(daemon.wait().unwrap().success());
    assert!(!socket.exists());
    assert!(stop().contains("no lodestone daemon running"));

    // SIGTERM while an indexing pass is running: the daemon exits promptly and cleanly, without
    // waiting for the pass (each embedding call takes 6 s).
    let mut daemon = serve("6000");
    wait_until("the daemon", Duration::from_secs(30), || {
        client(&home).is_some()
    });
    let c = client(&home).unwrap();
    c.register(&RegisterRequest {
        path: repo(3).to_string_lossy().into_owned(),
    })
    .unwrap();
    wait_until("indexing", Duration::from_secs(10), || {
        c.status(None).unwrap().indexing.running
    });
    let started = Instant::now();
    let killed = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    assert!(daemon.wait().unwrap().success(), "not a graceful exit");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(!socket.exists());
}

/// Calls one MCP tool through a `lode mcp` relay started in `dir`; returns (is_error, text).
fn relay_call(home: &Path, dir: &Path, tool: &str, arguments: Value) -> (bool, String) {
    use std::io::{BufRead, BufReader, Write};

    let mut relay = lode(home)
        .arg("mcp")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = relay.stdin.take().unwrap();
    let mut lines = BufReader::new(relay.stdout.take().unwrap()).lines();
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": "test", "version": "0" } } }),
    );
    rpc_response(&mut lines, 1);
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": tool, "arguments": arguments } }),
    );
    let out = rpc_response(&mut lines, 2);
    drop(stdin);
    relay.wait().unwrap();
    (
        out["result"]["isError"] == true,
        out["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

fn git_repo(n: usize) -> PathBuf {
    let root = repo(n);
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    root
}

#[test]
fn sessions_search_their_own_worktree_never_another_checkouts() {
    let root = git_repo(3);
    // A linked worktree inside the checkout, as agents create them, with its own changes.
    git(
        &root,
        &["worktree", "add", "-q", ".worktrees/feat", "-b", "feat"],
    );
    let feat = root.join(".worktrees/feat").canonicalize().unwrap();
    fs::remove_file(feat.join("core/src/m0.rs")).unwrap();
    write(
        &feat,
        "core/src/only_feat.rs",
        "/// Only on the feature branch.\npub fn only_in_feat() {}\n",
    );
    // An unrelated checkout nobody registered.
    let stray = git_repo(2);
    let home = on_demand_home(3);
    let ok = lode(&home)
        .args(["--local", "register"])
        .arg(&root)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok);

    // The session hook counts the worktree as served (and starts the daemon).
    let mut hook = lode(&home)
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = hook.stdin.take().unwrap();
        writeln!(stdin, "{}", json!({ "cwd": feat.join("core") })).unwrap();
    }
    let out = hook.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("additionalContext"),
        "{out:?}"
    );

    // A session in the worktree searches the worktree's own index, registered on first use.
    let search = |dir: &Path, query: &str| {
        relay_call(
            &home,
            dir,
            "semantic_code_search",
            json!({ "query": query }),
        )
    };
    wait_until("the worktree's index", Duration::from_secs(30), || {
        let (err, text) = search(&feat.join("core"), "only_in_feat");
        !err && text.contains("core/src/only_feat.rs")
    });
    let (_, text) = search(&feat.join("core"), "helper_0");
    assert!(!text.contains("core/src/m0.rs"), "{text}");
    let registry = fs::read_to_string(home.join("registry.json")).unwrap();
    assert!(registry.contains("\"auto\": true"), "{registry}");

    // The main checkout is unaffected by the worktree's changes.
    let (_, text) = search(&root, "helper_0");
    assert!(text.contains("core/src/m0.rs"), "{text}");
    let (_, text) = search(&root, "only_in_feat");
    assert!(!text.contains("only_feat.rs"), "{text}");

    // An unrelated checkout gets an error, not another checkout's results.
    let (err, text) = search(&stray, "helper_0");
    assert!(err && text.contains("lode register"), "{text}");
    let out = lode(&home)
        .args(["search", "helper_0"])
        .current_dir(&stray)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("lode register"),
        "{out:?}"
    );

    // Once the worktree is removed, the next daemon drops it and its index.
    let id = lodestone::index::worktree_id(&feat);
    assert!(home.join("worktrees").join(&id).exists());
    git(&root, &["worktree", "remove", "--force", ".worktrees/feat"]);
    assert!(lode(&home).arg("stop").status().unwrap().success());
    let out = lode(&home)
        .args(["search", "helper_1", "--repo"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let registry = fs::read_to_string(home.join("registry.json")).unwrap();
    assert!(!registry.contains("feat"), "{registry}");
    assert!(!home.join("worktrees").join(&id).exists());
    assert!(lode(&home).arg("stop").status().unwrap().success());
}

/// Reads JSON-RPC responses from the relay until the one with `id`.
fn rpc_response(lines: &mut impl Iterator<Item = std::io::Result<String>>, id: u64) -> Value {
    for line in lines {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if v["id"] == id {
            return v;
        }
    }
    panic!("relay closed before answering {id}");
}

#[test]
fn mcp_relay_starts_the_daemon_and_searches_the_session_worktree() {
    use std::io::{BufRead, BufReader, Write};

    let root = repo(5);
    // A second repository, so a call without `repo` is ambiguous unless the session decides.
    let other = repo(2);
    let home = on_demand_home(3);
    for r in [&other, &root] {
        let ok = lode(&home)
            .args(["--local", "register"])
            .arg(r)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    assert!(client(&home).is_none());

    let mut relay = lode(&home)
        .arg("mcp")
        .current_dir(root.join("core"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = relay.stdin.take().unwrap();
    let mut lines = BufReader::new(relay.stdout.take().unwrap()).lines();
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": "test", "version": "0" } } }),
    );
    assert_eq!(
        rpc_response(&mut lines, 1)["result"]["serverInfo"]["name"],
        "lodestone"
    );
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }));
    let tools = rpc_response(&mut lines, 2);
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    for t in tools {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
    }

    // No `repo`: the relay's session lease supplies the worktree it runs in.
    send(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "semantic_code_search", "arguments": { "query": "helper_3" } } }),
    );
    let out = rpc_response(&mut lines, 3);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(out["result"]["isError"], false, "{text}");
    assert!(text.contains("pub fn helper_3"), "{text}");
    assert!(text.contains("core/src/m3.rs"), "{text}");
    assert!(client(&home).is_some(), "the relay started a daemon");

    // Closing stdin ends the session; the daemon then exits after its idle timeout.
    drop(stdin);
    assert!(relay.wait().unwrap().success());
    wait_until("the idle daemon to exit", Duration::from_secs(30), || {
        client(&home).is_none()
    });
}
