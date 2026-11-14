//! One test per invariant in PLAN.md §3.1, plus the end-to-end index/search path, using a
//! deterministic hash embedder so they run without model downloads.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lodestone::api::types::SearchRequest;
use lodestone::config::{Config, SearchConfig};
use lodestone::embed::{Embedder, HashEmbedder, InputKind};
use lodestone::index::{IndexOptions, worktree_id};
use lodestone::registry::Engine;
use lodestone::search::{Filters, SearchParams};
use lodestone::testutil::tempdir;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// A small mixed-language Cargo workspace.
fn fixture() -> PathBuf {
    let root = tempdir().join("repo");
    write(
        &root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"core\", \"app\"]\n",
    );
    write(&root, "core/Cargo.toml", "[package]\nname = \"core\"\n");
    write(
        &root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\n[dependencies]\ncore = { path = \"../core\" }\n",
    );
    write(
        &root,
        "core/src/lib.rs",
        "/// Retry an operation with exponential backoff until a deadline.\npub fn retry_with_backoff() {}\n\n/// Copy a directory tree recursively.\npub fn copy_dir_all() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn copy_dir_all_works() {}\n}\n",
    );
    write(
        &root,
        "app/src/main.rs",
        "/// Parse the command line arguments.\nfn parse_args() {}\nfn main() {}\n",
    );
    write(
        &root,
        "web/ui.ts",
        "/** Render the login form. */\nfunction renderLogin() {}\nclass Session {}\n",
    );
    write(
        &root,
        "tools/gen.py",
        "def generate_report():\n    \"\"\"Generate the weekly report.\"\"\"\n    pass\n",
    );
    write(
        &root,
        "README.md",
        "# Fixture\n\nA test repository.\n\n## Usage\n\nRun it.\n",
    );
    write(&root, "notes.txt", "Plain notes about the project.\n");
    write(&root, "data.bin.json", "{\"a\": 1}\n");
    root
}

/// Counts embed calls so tests can assert on cache use.
struct CountingEmbedder {
    inner: HashEmbedder,
    texts: AtomicUsize,
    slow: AtomicBool,
}

impl Embedder for CountingEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn backend(&self) -> lodestone::config::Backend {
        self.inner.backend()
    }
    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        if kind == InputKind::Document {
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            if self.slow.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        self.inner.embed(texts, kind)
    }
}

fn engine(data: &Path) -> (Engine, Arc<CountingEmbedder>) {
    let mut config = Config {
        data_dir: Some(data.to_path_buf()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    let emb = Arc::new(CountingEmbedder {
        inner: HashEmbedder {
            dim: 64,
            id: "test/hash".into(),
            delay_ms: 0,
        },
        texts: AtomicUsize::new(0),
        slow: AtomicBool::new(false),
    });
    (Engine::with_embedder(config, emb.clone()).unwrap(), emb)
}

fn search(e: &Engine, repo: &Path, q: &str) -> lodestone::api::types::SearchResponse {
    e.search(&SearchRequest {
        query: q.into(),
        repo: Some(repo.to_string_lossy().to_string()),
        top_k: 8,
        ..Default::default()
    })
    .unwrap()
}

fn dir_bytes(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in walkdir(dir) {
        // Windows refuses to read a file another handle has byte-range locked (SQLite's); its
        // size and modification time stand in.
        let bytes = fs::read(&e).unwrap_or_else(|_| {
            let m = fs::metadata(&e).unwrap();
            format!("{} {:?}", m.len(), m.modified().unwrap()).into_bytes()
        });
        out.push((e.clone(), bytes));
    }
    out.sort();
    out
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn index_and_search_mixed_repo() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(r.items >= 9, "{r:?}");
    let resp = search(&e, &root, "retry with exponential backoff");
    assert_eq!(resp.results[0].name, "retry_with_backoff");
    assert!(!resp.stale, "{:?}", resp.stale_reasons);
    // Exact identifier match ranks first.
    assert_eq!(
        search(&e, &root, "copy_dir_all").results[0].name,
        "copy_dir_all"
    );
    // Test items are excluded by default.
    assert!(
        search(&e, &root, "copy_dir_all_works")
            .results
            .iter()
            .all(|r| !r.is_test)
    );
    // Every extension is accounted for, definitions tagged per language.
    let st = e.status(Some(&root.to_string_lossy())).unwrap();
    for ext in ["rs", "ts", "py", "md"] {
        assert!(st.extensions[ext].tagged > 0, "{ext}: {:?}", st.extensions);
    }
    assert!(st.extensions["txt"].text > 0);
    assert!(st.extensions["toml"].skipped_files > 0);
    assert!(st.extensions["json"].skipped_files > 0);
    // Layer filter: core cannot see app.
    let resp = e
        .search(&SearchRequest {
            query: "parse command line arguments".into(),
            repo: Some(root.to_string_lossy().to_string()),
            from_unit: Some("core".into()),
            top_k: 50,
            ..Default::default()
        })
        .unwrap();
    assert!(
        resp.results
            .iter()
            .all(|r| r.unit.as_deref() == Some("core")),
        "{:?}",
        resp.results
    );

    // Results carry the definition's full line range, in JSON and in the text line.
    let find = |q: &str, name: &str| {
        search(&e, &root, q)
            .results
            .into_iter()
            .find(|r| r.name == name)
            .unwrap()
    };
    let copy = find("copy a directory tree recursively", "copy_dir_all");
    assert_eq!((copy.line, copy.line_end), (5, 5));
    assert_eq!((copy.name_line, copy.name_column), (5, 8));
    assert!(
        copy.text.starts_with("core/src/lib.rs:5:8  lines 5-5  "),
        "{}",
        copy.text
    );
    let report = find("generate the weekly report", "generate_report");
    assert_eq!((report.line, report.line_end), (1, 3));
    assert_eq!((report.name_line, report.name_column), (1, 5));
    assert!(
        report.text.starts_with("tools/gen.py:1:5  lines 1-3  "),
        "{}",
        report.text
    );
}

/// Invariant 1: queries never index. A search against a missing index returns a stale flag and
/// writes nothing; a search against a stale index does not pick up the change.
#[test]
fn queries_never_index() {
    let root = fixture();
    let data = tempdir();
    let (e, emb) = engine(&data);
    // An unregistered root is reported, not searched (or indexed) on the spot.
    let err = e
        .search(&SearchRequest {
            query: "retry".into(),
            repo: Some(root.to_string_lossy().to_string()),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("lode register"), "{err:#}");
    let id = worktree_id(&fs::canonicalize(&root).unwrap());
    assert!(
        !data
            .join("worktrees")
            .join(&id)
            .join("manifest.json")
            .exists()
    );
    assert_eq!(emb.texts.load(Ordering::SeqCst), 0);

    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let embedded = emb.texts.load(Ordering::SeqCst);
    write(
        &root,
        "core/src/new.rs",
        "/// Brand new helper for frobnicating widgets.\npub fn frobnicate_widgets() {}\n",
    );
    let resp = search(&e, &root, "frobnicate_widgets");
    assert!(resp.results.iter().all(|r| r.name != "frobnicate_widgets"));
    assert_eq!(
        emb.texts.load(Ordering::SeqCst),
        embedded,
        "search must not embed documents"
    );
    let st = e.status(Some(&root.to_string_lossy())).unwrap();
    assert!(st.stale && st.pending_changes == 1, "{st:?}");
}

/// Invariant 2: one canonical key. Different spellings of the same root map to one worktree id
/// and one index; manifests store relative paths.
#[test]
fn one_canonical_key() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let dotted = root.join("core").join("..");
    let a = e.register(&root).unwrap();
    let c = e.register(&dotted).unwrap();
    assert!(Arc::ptr_eq(&a, &c));
    // Creating symlinks on Windows needs a privilege test runners may lack.
    #[cfg(unix)]
    {
        let link = root.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(Arc::ptr_eq(&a, &e.register(&link).unwrap()));
    }
    assert_eq!(e.repos().len(), 1);
    assert_eq!(a.id, worktree_id(&fs::canonicalize(&root).unwrap()));
    e.index_blocking(&a, IndexOptions::default()).unwrap();
    let m = a.manifest().unwrap();
    assert!(
        m.files
            .keys()
            .all(|k| !k.starts_with('/') && !k.contains(".."))
    );
    assert!(m.files.contains_key("core/src/lib.rs"));
}

/// An index stays open only while leased or in use, and a status overview opens none; a closed
/// one reopens with what it had, and indexes again after its writer was released.
#[test]
fn unused_indexes_are_closed_and_reopen_intact() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let items = w.snapshot().items.len();
    let kept = BTreeSet::from([w.root.clone()]);

    e.close_unused(&BTreeSet::new());
    assert_eq!(e.worktrees().len(), 1, "in use");
    w.release_writer();
    drop(w);
    e.close_unused(&kept);
    assert_eq!(e.worktrees().len(), 1, "leased");
    e.close_unused(&BTreeSet::new());
    assert!(e.worktrees().is_empty());

    assert_eq!(e.status_all().repos[0].items, items);
    assert!(e.worktrees().is_empty());

    write(&root, "core/src/more.rs", "pub fn one_more() {}\n");
    let w = e.register(&root).unwrap();
    assert_eq!(w.snapshot().items.len(), items);
    let report = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(report.files_changed, 1);
    assert_eq!(w.snapshot().items.len(), items + 1);
}

/// Invariant 3: single writer, snapshot readers. Searches during a reindex never block and see
/// either the old or the new snapshot, never a mix; an old snapshot stays valid after a swap.
#[test]
fn snapshot_readers_during_reindex() {
    let root = fixture();
    let data = tempdir();
    let (e, emb) = engine(&data);
    let e = Arc::new(e);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let old = w.snapshot();
    let old_count = old.items.len();
    for i in 0..40 {
        write(
            &root,
            &format!("core/src/gen{i}.rs"),
            &format!("/// Generated helper {i}.\npub fn generated_{i}() {{}}\n"),
        );
    }
    emb.slow.store(true, Ordering::SeqCst);
    let writer = {
        let (e, w) = (e.clone(), w.clone());
        std::thread::spawn(move || e.index_blocking(&w, IndexOptions::default()).unwrap())
    };
    while !w.is_writing() && !writer.is_finished() {
        std::thread::yield_now();
    }
    assert!(
        search(&e, &root, "generated helper").stale,
        "search during indexing must report stale"
    );
    let mut observed = std::collections::BTreeSet::new();
    while !writer.is_finished() {
        let t = std::time::Instant::now();
        let snap = w.snapshot();
        // Every vector hit and every lexical hit resolves to an item in the same snapshot.
        let resp = search(&e, &root, "generated helper");
        assert!(t.elapsed().as_millis() < 250, "search blocked on writer");
        for r in &resp.results {
            assert!(!r.path.is_empty());
        }
        let n = snap.items.len();
        assert!(n >= old_count);
        observed.insert(n);
    }
    writer.join().unwrap();
    assert!(
        observed.len() >= 2 || observed.contains(&old_count),
        "{observed:?}"
    );
    assert_eq!(old.items.len(), old_count, "old snapshot mutated");
    assert_eq!(w.snapshot().items.len(), old_count + 40);
    let resp = search(&e, &root, "generated_7");
    assert_eq!(resp.results[0].name, "generated_7");
}

/// Invariant 4: embeddings are a pure function of (model id, chunk text): identical text in a
/// second worktree is never re-embedded.
#[test]
fn embeddings_are_content_addressed() {
    let root = fixture();
    let copy = tempdir().join("copy");
    for f in walkdir(&root) {
        let rel = f.strip_prefix(&root).unwrap();
        write(
            &copy,
            &rel.to_string_lossy(),
            &fs::read_to_string(&f).unwrap(),
        );
    }
    let data = tempdir();
    let (e, emb) = engine(&data);
    let a = e.register(&root).unwrap();
    e.index_blocking(&a, IndexOptions::default()).unwrap();
    let first = emb.texts.load(Ordering::SeqCst);
    assert!(first > 0);
    let b = e.register(&copy).unwrap();
    let r = e.index_blocking(&b, IndexOptions::default()).unwrap();
    // Chunk text includes the repo-relative path, which is identical in both worktrees.
    assert_eq!(r.chunks_embedded, 0, "{r:?}");
    assert_eq!(emb.texts.load(Ordering::SeqCst), first);
    assert_eq!(r.chunks_cached, r.items);
}

/// Invariant 5: scope filters never touch index state.
#[test]
fn filters_leave_index_byte_identical() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let before = dir_bytes(&w.dir);
    let version = w.snapshot().version;
    for req in [
        SearchRequest {
            from_unit: Some("core".into()),
            ..Default::default()
        },
        SearchRequest {
            language: Some("python".into()),
            ..Default::default()
        },
        SearchRequest {
            paths: vec!["web/".into(), "!**/*.md".into()],
            ..Default::default()
        },
        SearchRequest {
            kinds: vec!["class".into()],
            include_tests: true,
            ..Default::default()
        },
    ] {
        let req = SearchRequest {
            query: "helper".into(),
            repo: Some(root.to_string_lossy().to_string()),
            top_k: 8,
            ..req
        };
        e.search(&req).unwrap();
    }
    assert_eq!(dir_bytes(&w.dir), before);
    assert_eq!(w.snapshot().version, version);
}

/// Filters apply inside both index searches: with one candidate per list, a filtered search still
/// finds an item that unfiltered results rank below many others.
#[test]
fn filters_apply_inside_index_searches() {
    let root = fixture();
    let noise: String = (0..20)
        .map(|i| format!("/// Generate the weekly report.\npub fn weekly_report_{i}() {{}}\n\n"))
        .collect();
    write(&root, "core/src/noise.rs", &noise);
    let data = tempdir();
    let mut config = Config {
        data_dir: Some(data.to_path_buf()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    config.search.candidates = 1;
    let e = Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap();
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let paths_of = |paths: &[&str]| -> anyhow::Result<Vec<String>> {
        Ok(e.search(&SearchRequest {
            query: "Generate the weekly report.".into(),
            repo: Some(root.to_string_lossy().to_string()),
            top_k: 1,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        })?
        .results
        .into_iter()
        .map(|r| r.path)
        .collect())
    };
    assert_ne!(paths_of(&[]).unwrap(), ["web/ui.ts"]);
    // A directory, a glob and exclusions all reach it; `*` stays within one directory.
    for paths in [
        &["web"][..],
        &["**/*.ts"],
        &["!core", "!app", "!tools", "!*.md", "!*.txt"],
    ] {
        assert_eq!(paths_of(paths).unwrap(), ["web/ui.ts"], "{paths:?}");
    }
    assert!(paths_of(&["*.ts"]).unwrap().is_empty());
    let err = paths_of(&["src/[a"]).unwrap_err().to_string();
    assert!(err.contains("invalid path glob `src/[a`"), "{err}");
}

#[test]
fn reindex_no_changes_is_noop_and_edits_are_incremental() {
    let root = fixture();
    let data = tempdir();
    let (e, emb) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let n = emb.texts.load(Ordering::SeqCst);
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!((r.files_changed, r.chunks_embedded), (0, 0));
    write(
        &root,
        "core/src/lib.rs",
        "/// Totally different helper.\npub fn different_helper() {}\n",
    );
    fs::remove_file(root.join("tools/gen.py")).unwrap();
    let r = e
        .index_blocking(
            &w,
            IndexOptions {
                force: false,
                paths: Some(vec!["core/src/lib.rs".into(), "tools/gen.py".into()]),
            },
        )
        .unwrap();
    assert_eq!((r.files_changed, r.files_removed), (1, 1));
    assert_eq!(emb.texts.load(Ordering::SeqCst), n + 1);
    assert_eq!(
        search(&e, &root, "different_helper").results[0].name,
        "different_helper"
    );
    assert!(
        search(&e, &root, "generate_report")
            .results
            .iter()
            .all(|r| r.name != "generate_report")
    );
    assert!(
        search(&e, &root, "retry_with_backoff")
            .results
            .iter()
            .all(|r| r.name != "retry_with_backoff")
    );
}

/// Changing an index-time chunking setting re-chunks unchanged files on the next full index,
/// re-using cached embeddings for chunks whose text did not change.
#[test]
fn chunking_settings_change_rechunks() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    let first = e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    let mut config = Config {
        data_dir: Some(data.clone()),
        module_chunks: true,
        ..Config::default()
    };
    config.rerank.enabled = false;
    let emb = Arc::new(HashEmbedder {
        dim: 64,
        id: "test/hash".into(),
        delay_ms: 0,
    });
    let e = Engine::with_embedder(config, emb).unwrap();
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, first.files_changed, "{r:?}");
    assert!(
        r.chunks_embedded > 0 && r.chunks_cached >= first.items,
        "{r:?}"
    );
    assert!(w.snapshot().items.values().any(|i| i.item.kind == "module"));
    let again = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(again.files_changed, 0);
}

fn hash_embedder() -> HashEmbedder {
    HashEmbedder {
        dim: 64,
        id: "test/hash".into(),
        delay_ms: 0,
    }
}

fn doc_engine(data: &Path, doc_vectors: bool) -> Engine {
    let mut config = Config {
        data_dir: Some(data.to_path_buf()),
        doc_vectors,
        ..Config::default()
    };
    config.rerank.enabled = false;
    Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap()
}

/// Names ranked by the doc-vector list alone (code-vector weight 0).
fn doc_only_names(w: &lodestone::index::WorktreeIndex, query: &str) -> Vec<String> {
    let q = hash_embedder()
        .embed(&[query], InputKind::Query)
        .unwrap()
        .remove(0);
    let config = SearchConfig {
        vector_weight: 0.0,
        doc_weight: 1.0,
        ..SearchConfig::default()
    };
    let snap = w.snapshot();
    let out = lodestone::search::search(
        &snap,
        w.lexical(),
        &SearchParams {
            query,
            query_vector: Some(&q),
            sentence_vectors: &[],
            filters: &Filters::default(),
            top_k: 8,
            config: &config,
        },
    )
    .unwrap();
    out.hits.iter().map(|h| h.row.item.name.clone()).collect()
}

/// Doc vectors exist exactly for documented items: turning the setting on fills them on the
/// next index, edits replace them, search fuses the doc list, and the engine ignores it while
/// the reranker is off.
#[test]
fn doc_vectors_follow_documented_items() {
    let root = fixture();
    let data = tempdir();
    let e = doc_engine(&data, false);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(w.snapshot().doc_vectors.is_empty());
    drop((e, w));

    let e = doc_engine(&data, true);
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(r.chunks_embedded > 0, "{r:?}");
    let check = |w: &lodestone::index::WorktreeIndex| {
        let snap = w.snapshot();
        let documented: Vec<u64> = snap
            .items
            .values()
            .filter(|r| r.item.doc_text().is_some())
            .map(|r| r.id)
            .collect();
        assert!(!documented.is_empty());
        assert_eq!(snap.doc_vectors.len(), documented.len());
        assert!(documented.iter().all(|id| snap.doc_vectors.contains(*id)));
    };
    check(&w);
    let again = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!((again.files_changed, again.chunks_embedded), (0, 0));

    let names = doc_only_names(&w, "copy a directory tree recursively");
    assert_eq!(names[0], "copy_dir_all");
    assert!(!names.iter().any(|n| n == "main"));
    // Without a reranker the engine searches code vectors only, so undocumented items match.
    assert!(
        search(&e, &root, "fn main")
            .results
            .iter()
            .any(|r| r.name == "main")
    );

    write(
        &root,
        "core/src/lib.rs",
        "pub fn undocumented_helper() {}\n",
    );
    e.index_blocking(
        &w,
        IndexOptions {
            force: false,
            paths: Some(vec!["core/src/lib.rs".into()]),
        },
    )
    .unwrap();
    check(&w);
    assert!(
        !doc_only_names(&w, "copy a directory tree recursively")
            .iter()
            .any(|n| n == "copy_dir_all")
    );
    drop((e, w));

    // Reopening loads the persisted doc index.
    let e = doc_engine(&data, true);
    let w = e.register(&root).unwrap();
    check(&w);
    assert!(!w.snapshot().stale.iter().any(|s| s.contains("no vector")));
}
