//! One test per invariant in docs/architecture.md, plus the end-to-end index/search path, using a
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
use lodestone::watch::Watchers;

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

/// A change to one extension's chunking (a user query overlay, a remapped extension) re-chunks
/// only that extension's files on the next full index.
#[test]
fn language_change_rechunks_only_its_files() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));

    write(&data, "queries/python/tags.scm", "; user overlay\n");
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, 1, "only tools/gen.py: {r:?}");
    drop((e, w));

    let mut config = Config {
        data_dir: Some(data.clone()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    config
        .languages
        .extensions
        .insert("txt".into(), "python".into());
    let e = Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap();
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, 1, "only notes.txt: {r:?}");
    assert!(
        w.snapshot()
            .items
            .values()
            .any(|i| i.item.rel_path == "notes.txt" && i.item.language == "python")
    );
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
        &w.lexical(),
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

/// A crash between a deletion's commit and the next vector checkpoint leaves the deleted items'
/// vectors on disk. Repair removes them, so their ids can be allocated again.
#[test]
fn repair_removes_vectors_of_deleted_items() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    // The file holding the highest ids: deleting it frees them for reuse.
    let last = {
        let snap = w.snapshot();
        let id = snap.items.keys().max().copied().unwrap();
        snap.items[&id].item.rel_path.clone()
    };
    let vectors = w.dir.join("vectors.usearch");
    let checkpoint = fs::read(&vectors).unwrap();
    fs::remove_file(root.join(&last)).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    fs::write(&vectors, checkpoint).unwrap();

    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    assert!(
        w.snapshot()
            .stale
            .iter()
            .any(|s| s.contains("deleted items"))
    );
    write(&root, "core/src/new.rs", "pub fn freshly_added() {}\n");
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let snap = w.snapshot();
    assert_eq!(snap.vectors.len(), snap.items.len());
    assert_eq!(
        search(&e, &root, "freshly_added").results[0].name,
        "freshly_added"
    );
}

/// A repository nested in a registered one is served by its own index; a submodule belongs to
/// the repository that contains it.
#[test]
fn nested_checkouts_are_left_to_their_own_index() {
    let root = fixture();
    fs::create_dir_all(root.join(".git/modules/sub")).unwrap();
    write(&root, "checkouts/other/.git/HEAD", "ref: refs/heads/main\n");
    write(
        &root,
        "checkouts/other/other.rs",
        "pub fn other_checkout() {}\n",
    );
    write(&root, "sub/.git", "gitdir: ../.git/modules/sub\n");
    write(&root, "sub/sub.rs", "pub fn in_submodule() {}\n");
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let names: BTreeSet<String> = w
        .snapshot()
        .items
        .values()
        .map(|r| r.item.name.clone())
        .collect();
    assert!(names.contains("in_submodule"), "{names:?}");
    assert!(!names.contains("other_checkout"), "{names:?}");
}

/// A subtree the walk cannot read keeps its indexed files instead of losing them as deleted.
#[cfg(unix)]
#[test]
fn an_unreadable_subtree_keeps_its_index() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let items = w.snapshot().items.len();
    let web = root.join("web");
    let repo = root.to_string_lossy().to_string();
    fs::set_permissions(&web, fs::Permissions::from_mode(0o000)).unwrap();
    let report = e.index_blocking(&w, IndexOptions::default());
    // A forced rebuild keeps what it cannot read again.
    let forced = e.index_blocking(
        &w,
        IndexOptions {
            force: true,
            paths: None,
        },
    );
    let status = e.status(Some(&repo)).unwrap();
    fs::set_permissions(&web, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(report.unwrap().files_removed, 0);
    assert_eq!(forced.unwrap().files_removed, 0);
    assert_eq!(w.snapshot().items.len(), items);
    assert_eq!(status.pending_changes, 0);
    assert!(
        status
            .stale_reasons
            .iter()
            .any(|r| r.contains("could not read web")),
        "{status:?}"
    );
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(!e.status(Some(&repo)).unwrap().stale);
}

/// A file that cannot be read when its turn comes keeps its items through edits and rebuilds
/// (forced, or for another model), and the index says so, also after reopening, until it is read.
#[cfg(unix)]
#[test]
fn an_unreadable_file_keeps_its_items_and_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let repo = root.to_string_lossy().to_string();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let items = w.snapshot().items.len();
    let file = root.join("tools/gen.py");
    write(&root, "tools/gen.py", "def changed():\n    pass\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let has_report = |w: &lodestone::index::WorktreeIndex| {
        w.snapshot()
            .items
            .values()
            .any(|r| r.item.name == "generate_report")
    };
    let forced = e.index_blocking(
        &w,
        IndexOptions {
            force: true,
            paths: None,
        },
    );
    let kept_forced = has_report(&w);
    let incremental = e.index_blocking(&w, IndexOptions::default());
    drop((e, w));
    let (e, _) = engine(&data);
    let reopened = e.status(Some(&repo)).unwrap();
    drop(e);
    // Another model: the file's items stay for lexical search.
    let other = || {
        let mut config = Config {
            data_dir: Some(data.clone()),
            ..Config::default()
        };
        config.rerank.enabled = false;
        let embedder = HashEmbedder {
            dim: 64,
            id: "test/other".into(),
            delay_ms: 0,
        };
        Engine::with_embedder(config, Arc::new(embedder)).unwrap()
    };
    let e = other();
    let w = e.register(&root).unwrap();
    let new_model = e.index_blocking(&w, IndexOptions::default());
    let kept_new_model = has_report(&w);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    forced.unwrap();
    incremental.unwrap();
    new_model.unwrap();
    assert!(kept_forced && kept_new_model);
    assert_eq!(reopened.items, items);
    assert!(
        reopened
            .stale_reasons
            .iter()
            .any(|r| r.contains("could not read tools/gen.py")),
        "{reopened:?}"
    );
    assert_eq!(
        e.index_blocking(&w, IndexOptions::default())
            .unwrap()
            .files_changed,
        1
    );
    assert!(!e.status(Some(&repo)).unwrap().stale);
    assert!(!has_report(&w));
}

/// A file that could not be read, then deleted, is no longer reported once a watcher pass sees it
/// gone.
#[cfg(unix)]
#[test]
fn a_deleted_unreadable_file_is_no_longer_reported() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let repo = root.to_string_lossy().to_string();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let file = root.join("tools/gen.py");
    write(&root, "tools/gen.py", "def changed():\n    pass\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let scoped = || IndexOptions {
        force: false,
        paths: Some(vec!["tools/gen.py".into()]),
    };
    e.index_blocking(&w, scoped()).unwrap();
    assert!(e.status(Some(&repo)).unwrap().stale);
    fs::remove_file(&file).unwrap();
    assert_eq!(e.index_blocking(&w, scoped()).unwrap().files_removed, 1);
    assert!(!e.status(Some(&repo)).unwrap().stale);
}

/// An engine on `data` embedding as `id` (a model other than `engine`'s when not "test/hash").
fn engine_as(data: &Path, id: &str) -> Engine {
    let mut config = Config {
        data_dir: Some(data.to_path_buf()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    let embedder = HashEmbedder {
        dim: 64,
        id: id.into(),
        delay_ms: 0,
    };
    Engine::with_embedder(config, Arc::new(embedder)).unwrap()
}

/// Items kept through a model change (their file unreadable) get no vector of the old model from
/// the cache, and stay findable by name.
#[cfg(unix)]
#[test]
fn items_kept_through_a_model_change_get_no_old_vector() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let e = engine_as(&data, "test/hash");
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    let file = root.join("tools/gen.py");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let e = engine_as(&data, "test/other");
    let w = e.register(&root).unwrap();
    let migrated = e.index_blocking(&w, IndexOptions::default());
    drop((e, w));
    // Reopened: the first pass repairs missing vectors from the cache.
    let e = engine_as(&data, "test/other");
    let w = e.register(&root).unwrap();
    let repaired = e.index_blocking(&w, IndexOptions::default());
    let snap = w.snapshot();
    let kept: Vec<u64> = snap
        .items
        .values()
        .filter(|r| r.item.rel_path == "tools/gen.py")
        .map(|r| r.id)
        .collect();
    let found = search(&e, &root, "generate_report");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    migrated.unwrap();
    repaired.unwrap();
    assert!(!kept.is_empty());
    assert!(kept.iter().all(|id| !snap.vectors.contains(*id)));
    assert_eq!(found.results[0].name, "generate_report");
}

/// Embeds nothing: a model change that fails part-way.
struct FailingEmbedder;

impl Embedder for FailingEmbedder {
    fn model_id(&self) -> &str {
        "test/other"
    }
    fn dim(&self) -> usize {
        64
    }
    fn backend(&self) -> lodestone::config::Backend {
        lodestone::config::Backend::Remote
    }
    fn embed(&self, _: &[&str], _: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("embedding failed")
    }
}

/// A model change that fails after saving the manifest never leaves the old model's vectors on
/// disk under the new model's name.
#[test]
fn a_failed_model_change_leaves_no_old_vectors_under_the_new_model() {
    let root = fixture();
    let data = tempdir();
    let e = engine_as(&data, "test/hash");
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    // A removal is committed (and the manifest saved) before embedding fails.
    fs::remove_file(root.join("notes.txt")).unwrap();
    let mut config = Config {
        data_dir: Some(data.clone()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    let e = Engine::with_embedder(config.clone(), Arc::new(FailingEmbedder)).unwrap();
    let w = e.register(&root).unwrap();
    assert!(e.index_blocking(&w, IndexOptions::default()).is_err());
    // A watcher pass for an unrelated file resumes the rebuild rather than calling it done.
    let repo = root.to_string_lossy().to_string();
    write(&root, "core/src/empty.rs", "");
    let scoped = || IndexOptions {
        force: false,
        paths: Some(vec!["core/src/empty.rs".into()]),
    };
    assert!(e.index_blocking(&w, scoped()).is_err());
    assert!(e.status(Some(&repo)).unwrap().stale);
    drop((e, w));
    // Restarted during the outage: the first pass repairs, then fails again. Searches still say
    // the index is being rebuilt.
    let e = Engine::with_embedder(config.clone(), Arc::new(FailingEmbedder)).unwrap();
    let w = e.register(&root).unwrap();
    assert!(e.index_blocking(&w, scoped()).is_err());
    let resp = search(&e, &root, "retry_with_backoff");
    assert!(resp.stale, "{resp:?}");
    assert!(
        resp.stale_reasons.iter().any(|r| r.contains("rebuild")),
        "{resp:?}"
    );
    drop((e, w));
    let e = engine_as(&data, "test/other");
    let w = e.register(&root).unwrap();
    assert_eq!(w.snapshot().vectors.len(), 0);
    let status = e.status(Some(&repo)).unwrap();
    assert!(
        status.stale_reasons.iter().any(|r| r.contains("rebuild")),
        "{status:?}"
    );
    e.index_blocking(&w, scoped()).unwrap();
    let snap = w.snapshot();
    assert_eq!(snap.vectors.len(), snap.items.len());
    assert!(!e.status(Some(&repo)).unwrap().stale);
    assert_eq!(
        search(&e, &root, "retry with exponential backoff").results[0].name,
        "retry_with_backoff"
    );
}

/// A pass that fails under the same model (the embedding service down for indexing), forced or
/// not, leaves searches saying so until a pass succeeds.
#[test]
fn a_failed_pass_leaves_searches_stale_until_one_succeeds() {
    let root = fixture();
    let data = tempdir();
    let flag = data.join("down");
    let mut config = Config {
        data_dir: Some(data.clone()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    let embedder = lodestone::testutil::FailWhile {
        inner: Arc::new(hash_embedder()),
        flag: flag.clone(),
    };
    let e = Engine::with_embedder(config, Arc::new(embedder)).unwrap();
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    write(
        &root,
        "core/src/lib.rs",
        "/// Retry with a new name.\npub fn renamed_retry() {}\n",
    );
    fs::write(&flag, "").unwrap();
    for force in [false, true] {
        let opts = IndexOptions { force, paths: None };
        assert!(e.index_blocking(&w, opts).is_err());
        let resp = search(&e, &root, "retry_with_backoff");
        assert!(resp.stale, "force {force}: {resp:?}");
        assert!(
            resp.stale_reasons.iter().any(|r| r.contains("failed")),
            "force {force}: {resp:?}"
        );
    }
    fs::remove_file(&flag).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let resp = search(&e, &root, "renamed_retry");
    assert!(!resp.stale, "{resp:?}");
    assert_eq!(resp.results[0].name, "renamed_retry");
    assert!(
        search(&e, &root, "retry_with_backoff")
            .results
            .iter()
            .all(|r| r.name != "retry_with_backoff")
    );
}

/// What a failed pass was to index is recorded with the index: another process reports it, and
/// its next pass, even one for another file, indexes it.
#[test]
fn a_failed_pass_is_retried_by_the_next_one_in_any_process() {
    for failed_scope in [None, Some(vec!["core/src/lib.rs".to_string()])] {
        let root = fixture();
        let data = tempdir();
        let flag = data.join("down");
        let repo = root.to_string_lossy().to_string();
        let mut config = Config {
            data_dir: Some(data.clone()),
            ..Config::default()
        };
        config.rerank.enabled = false;
        let flaky = || {
            let embedder = lodestone::testutil::FailWhile {
                inner: Arc::new(hash_embedder()),
                flag: flag.clone(),
            };
            Engine::with_embedder(config.clone(), Arc::new(embedder)).unwrap()
        };
        let e = flaky();
        let w = e.register(&root).unwrap();
        e.index_blocking(&w, IndexOptions::default()).unwrap();
        drop((e, w));
        write(
            &root,
            "core/src/lib.rs",
            "/// Retry with a new name.\npub fn renamed_retry() {}\n",
        );
        fs::write(&flag, "").unwrap();
        let e = flaky();
        let w = e.register(&root).unwrap();
        let opts = IndexOptions {
            force: false,
            paths: failed_scope.clone(),
        };
        assert!(e.index_blocking(&w, opts).is_err());
        drop((e, w));
        fs::remove_file(&flag).unwrap();
        // Another process: it says so, and its next pass, for another file, retries the change.
        let e = flaky();
        let status = e.status(Some(&repo)).unwrap();
        assert!(
            status.stale_reasons.iter().any(|r| r.contains("retries")),
            "{failed_scope:?}: {status:?}"
        );
        let w = e.register(&root).unwrap();
        write(&root, "notes.txt", "Other notes.\n");
        let opts = IndexOptions {
            force: false,
            paths: Some(vec!["notes.txt".into()]),
        };
        e.index_blocking(&w, opts).unwrap();
        let resp = search(&e, &root, "renamed_retry");
        assert!(!resp.stale, "{failed_scope:?}: {resp:?}");
        assert_eq!(resp.results[0].name, "renamed_retry", "{failed_scope:?}");
    }
}

/// A batch whose lexical write failed is fully replaced by the retry, in the running index: no
/// obsolete item or vector stays.
#[cfg(unix)]
#[test]
fn a_retry_replaces_a_batch_whose_lexical_write_failed() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let (items, vectors) = (w.snapshot().items.len(), w.snapshot().vectors.len());
    write(
        &root,
        "core/src/lib.rs",
        "/// Retry with a new name.\npub fn renamed_retry() {}\n\n/// Copy a directory tree recursively.\npub fn copy_dir_all() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn copy_dir_all_works() {}\n}\n",
    );
    let lexical = w.dir.join("lexical");
    fs::set_permissions(&lexical, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = e.index_blocking(&w, IndexOptions::default());
    fs::set_permissions(&lexical, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err());
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let snap = w.snapshot();
    assert_eq!((snap.items.len(), snap.vectors.len()), (items, vectors));
    let names: Vec<&str> = snap.items.values().map(|r| r.item.name.as_str()).collect();
    assert!(!names.contains(&"retry_with_backoff"), "{names:?}");
    assert!(names.contains(&"renamed_retry"), "{names:?}");
    let resp = search(&e, &root, "retry_with_backoff");
    assert!(!resp.stale, "{resp:?}");
    assert!(resp.results.iter().all(|r| r.name != "retry_with_backoff"));
}

/// Rows of `rel_path` in a worktree's `items.sqlite`.
fn stored_rows(dir: &Path, rel_path: &str) -> i64 {
    rusqlite::Connection::open(dir.join("items.sqlite"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM items WHERE rel_path = ?1",
            [rel_path],
            |r| r.get(0),
        )
        .unwrap()
}

/// A deletion whose lexical write fails leaves both stores as they were. Restoring the file and
/// adding another, then indexing with a reopened writer, keeps both files' items, each id its own.
#[cfg(unix)]
#[test]
fn a_failed_deletion_changes_neither_store_and_ids_stay_unique() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let items = w.snapshot().items.len();
    let file = root.join("tools/gen.py");
    let bytes = fs::read(&file).unwrap();
    fs::remove_file(&file).unwrap();
    let lexical = w.dir.join("lexical");
    fs::set_permissions(&lexical, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = e.index_blocking(&w, IndexOptions::default());
    fs::set_permissions(&lexical, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err());
    assert!(stored_rows(&w.dir, "tools/gen.py") > 0, "rolled back");
    fs::write(&file, bytes).unwrap();
    write(&root, "core/src/new.rs", "pub fn brand_new() {}\n");
    w.release_writer();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let snap = w.snapshot();
    assert_eq!(snap.items.len(), items + 1);
    assert_eq!(snap.vectors.len(), snap.items.len());
    for name in ["generate_report", "brand_new"] {
        let resp = search(&e, &root, name);
        assert!(!resp.stale, "{resp:?}");
        assert_eq!(resp.results[0].name, name);
    }
}

/// A path a failed batch may have half-written (recorded for retry) is indexed again even though
/// its stat and hash are unchanged.
#[test]
fn a_recorded_path_is_indexed_again_whatever_its_stat() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let dir = w.dir.clone();
    drop((e, w));
    let path = dir.join("manifest.json");
    let mut manifest = lodestone::index::Manifest::load(&path).unwrap().unwrap();
    manifest.retry_paths = vec!["tools/gen.py".into()];
    manifest.save(&path).unwrap();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, 1);
    assert!(
        lodestone::index::Manifest::load(&path)
            .unwrap()
            .unwrap()
            .retry_paths
            .is_empty()
    );
}

/// An engine on `data` searching by keyword only: a vector search returns nearest items for any
/// query.
fn keyword_engine(data: &Path) -> Engine {
    keyword_engine_with(data, lodestone::index::TokenizerKind::Code)
}

/// [`keyword_engine`] with the lexical `tokenizer`.
fn keyword_engine_with(data: &Path, tokenizer: lodestone::index::TokenizerKind) -> Engine {
    let mut config = Config {
        data_dir: Some(data.to_path_buf()),
        lexical_tokenizer: tokenizer,
        ..Config::default()
    };
    config.rerank.enabled = false;
    config.search.lexical_weight = 1.0;
    config.search.identifier_lexical_weight = 1.0;
    config.search.vector_weight = 0.0;
    config.search.sentence_weight = 0.0;
    Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap()
}

/// What a crash between a batch's lexical and SQLite commits leaves in the index at `dir`: a
/// lexical entry for `rel_path` (a function `name`) under the id the next item would get, and the
/// journal naming the path.
fn crash_between_commits(dir: &Path, rel_path: &str, name: &str) {
    use lodestone::chunk::Item;
    use lodestone::index::{ItemRow, ItemStore, Lexical, TokenizerKind};

    let next = ItemStore::open(&dir.join("items.sqlite"))
        .unwrap()
        .max_id()
        .unwrap()
        + 1;
    let lexical = Lexical::open(&dir.join("lexical"), TokenizerKind::Code).unwrap();
    let mut writer = lexical.writer().unwrap();
    let row = ItemRow {
        id: next,
        item: Item {
            rel_path: rel_path.into(),
            language: "rust".into(),
            kind: "function".into(),
            name: name.into(),
            qualified_name: name.into(),
            line_start: 1,
            line_end: 1,
            name_line: 1,
            name_column: 4,
            signature: format!("fn {name}()"),
            doc: String::new(),
            is_test: false,
            tagged: true,
            text: format!("fn {name}() {{}}"),
        },
        unit: None,
        chunk_key: [0; 32],
    };
    lexical.add(&writer, &row).unwrap();
    writer.commit().unwrap();
    fs::write(dir.join("batch.json"), format!("[\"{rel_path}\"]")).unwrap();
}

/// A crash between the lexical and SQLite commits leaves a lexical-only entry, which no store
/// but the journal knows. The next pass deletes it, and the id another file gets next is not its
/// id, so it matches nothing.
#[test]
fn a_lexical_only_entry_from_a_crashed_batch_is_deleted() {
    let root = fixture();
    let data = tempdir();
    let e = keyword_engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let dir = w.dir.clone();
    drop((e, w));
    crash_between_commits(&dir, "ghost.rs", "xyloquokka");

    let e = keyword_engine(&data);
    let repo = root.to_string_lossy().to_string();
    assert!(e.status(Some(&repo)).unwrap().stale);
    let w = e.register(&root).unwrap();
    write(&root, "core/src/current.rs", "pub fn elephant() {}\n");
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_removed, 1);
    let resp = search(&e, &root, "xyloquokka");
    assert!(!resp.stale, "{resp:?}");
    assert!(resp.results.is_empty(), "{resp:?}");
    assert_eq!(search(&e, &root, "elephant").results[0].name, "elephant");
}

/// The crashed batch's file is unreadable when the next pass runs, then deleted: its lexical entry
/// matches nothing meanwhile, its path stays recorded, and the pass that finds it gone removes it.
#[cfg(unix)]
#[test]
fn a_crashed_batch_for_an_unreadable_file_never_matches_another() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let e = keyword_engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    // The same index stays open (as in a daemon, where no reopening repair runs); only its
    // writer goes, as between watcher passes.
    w.release_writer();
    write(&root, "ghost.rs", "fn xyloquokka() {}\n");
    let ghost = root.join("ghost.rs");
    fs::set_permissions(&ghost, fs::Permissions::from_mode(0o000)).unwrap();
    crash_between_commits(&w.dir, "ghost.rs", "xyloquokka");

    let repo = root.to_string_lossy().to_string();
    write(&root, "core/src/current.rs", "pub fn elephant() {}\n");
    let pass = e.index_blocking(&w, IndexOptions::default());
    fs::remove_file(&ghost).unwrap();
    pass.unwrap();
    let resp = search(&e, &root, "xyloquokka");
    assert!(
        resp.results.iter().all(|r| r.name != "elephant"),
        "{resp:?}"
    );
    assert!(resp.stale, "still recorded: {resp:?}");
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_removed, 1);
    let resp = search(&e, &root, "xyloquokka");
    assert!(!resp.stale && resp.results.is_empty(), "{resp:?}");
    assert!(!e.status(Some(&repo)).unwrap().stale);
}

/// Switching the lexical tokenizer and back never serves the entries the other left behind: a
/// deleted function's name matches nothing, before the next index (keyword results are left out,
/// and the index says why) and after it, and no id is given out twice.
#[test]
fn switching_tokenizers_never_serves_stale_entries() {
    use lodestone::index::TokenizerKind::{Code, Default};

    let root = fixture();
    let data = tempdir();
    let index = |tokenizer| {
        let e = keyword_engine_with(&data, tokenizer);
        let w = e.register(&root).unwrap();
        e.index_blocking(&w, IndexOptions::default()).unwrap();
        (e, w)
    };
    let id_of = |w: &lodestone::index::WorktreeIndex, name: &str| {
        w.snapshot()
            .items
            .values()
            .find(|r| r.item.name == name)
            .map(|r| r.id)
            .unwrap()
    };
    write(&root, "core/src/ghost.rs", "pub fn xyloquokka() {}\n");
    let (_, w) = index(Code);
    let ghost = id_of(&w, "xyloquokka");
    drop(w);
    fs::remove_file(root.join("core/src/ghost.rs")).unwrap();
    drop(index(Default));
    write(&root, "core/src/current.rs", "pub fn elephant() {}\n");
    let (_, w) = index(Default);
    assert!(id_of(&w, "elephant") > ghost, "ids are never reused");
    drop(w);
    // Back to code, searched before any index runs.
    let e = keyword_engine_with(&data, Code);
    let resp = search(&e, &root, "xyloquokka");
    assert!(resp.results.is_empty(), "{resp:?}");
    assert!(
        resp.stale_reasons.iter().any(|r| r.contains("tokenizer")),
        "{resp:?}"
    );
    drop(e);
    let (e, _) = index(Code);
    let resp = search(&e, &root, "xyloquokka");
    assert!(resp.results.is_empty() && !resp.stale, "{resp:?}");
    assert_eq!(search(&e, &root, "elephant").results[0].name, "elephant");
}

/// A long-lived index (a daemon's) takes up what another process's writer committed while it did
/// not hold the lock, rather than deciding from its old view.
#[test]
fn another_writers_commits_are_taken_up() {
    use lodestone::index::TokenizerKind::{Code, Default};

    let root = fixture();
    let data = tempdir();
    let daemon = keyword_engine_with(&data, Code);
    let w = daemon.register(&root).unwrap();
    daemon.index_blocking(&w, IndexOptions::default()).unwrap();
    w.release_writer();
    write(
        &root,
        "core/src/lib.rs",
        "/// Renamed.\npub fn harborpencil() {}\n",
    );
    let cli = keyword_engine_with(&data, Default);
    let wc = cli.register(&root).unwrap();
    cli.index_blocking(&wc, IndexOptions::default()).unwrap();
    drop((cli, wc));
    daemon.index_blocking(&w, IndexOptions::default()).unwrap();
    let resp = search(&daemon, &root, "harborpencil");
    assert!(!resp.stale, "{resp:?}");
    assert_eq!(resp.results[0].name, "harborpencil");
    assert!(
        w.snapshot()
            .items
            .values()
            .all(|r| r.item.name != "retry_with_backoff")
    );
}

/// A failed read of another writer's commits is read again by the next pass, not taken as done.
#[cfg(unix)]
#[test]
fn a_failed_reload_is_retried() {
    use lodestone::index::TokenizerKind::{Code, Default};
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let daemon = keyword_engine_with(&data, Code);
    let w = daemon.register(&root).unwrap();
    daemon.index_blocking(&w, IndexOptions::default()).unwrap();
    w.release_writer();
    write(
        &root,
        "core/src/lib.rs",
        "/// Renamed.\npub fn harborpencil() {}\n",
    );
    let cli = keyword_engine_with(&data, Default);
    let wc = cli.register(&root).unwrap();
    cli.index_blocking(&wc, IndexOptions::default()).unwrap();
    drop((cli, wc));
    let items = w.dir.join("items.sqlite");
    fs::set_permissions(&items, fs::Permissions::from_mode(0o000)).unwrap();
    let failed = daemon.index_blocking(&w, IndexOptions::default());
    fs::set_permissions(&items, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(failed.is_err());
    w.release_writer();
    daemon.index_blocking(&w, IndexOptions::default()).unwrap();
    let resp = search(&daemon, &root, "harborpencil");
    assert!(!resp.stale, "{resp:?}");
    assert_eq!(resp.results[0].name, "harborpencil");
}

/// A writer using the other tokenizer (its own lexical directory) cannot take the worktree's
/// writer lock either.
#[test]
fn one_writer_lock_covers_every_tokenizer() {
    let root = fixture();
    let data = tempdir();
    let a = keyword_engine_with(&data, lodestone::index::TokenizerKind::Code);
    let wa = a.register(&root).unwrap();
    a.index_blocking(&wa, IndexOptions::default()).unwrap();
    let journal = wa.dir.join("batch.json");
    fs::write(&journal, "[\"core/src/lib.rs\"]").unwrap();
    let b = keyword_engine_with(&data, lodestone::index::TokenizerKind::Default);
    let wb = b.register(&root).unwrap();
    let err = b.index_blocking(&wb, IndexOptions::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("another lodestone process"),
        "{err:#}"
    );
    assert!(journal.exists());
}

/// A writer that cannot take the index's writer lock leaves the batch journal of the one that
/// holds it alone.
#[test]
fn only_the_lock_holder_recovers_the_batch_journal() {
    let root = fixture();
    let data = tempdir();
    let (a, _) = engine(&data);
    let wa = a.register(&root).unwrap();
    a.index_blocking(&wa, IndexOptions::default()).unwrap();
    // `a` keeps its writer, and its lock, between passes; a batch of its is in flight.
    let journal = wa.dir.join("batch.json");
    fs::write(&journal, "[\"core/src/lib.rs\"]").unwrap();
    let (b, _) = engine(&data);
    let wb = b.register(&root).unwrap();
    assert!(b.index_blocking(&wb, IndexOptions::default()).is_err());
    assert!(journal.exists());
}

/// Items stored but never published (left by a failed batch) are removed once their file is
/// found gone.
#[test]
fn stored_items_of_a_missing_file_are_removed() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    rusqlite::Connection::open(w.dir.join("items.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO items (id, rel_path, language, kind, name, qualified_name, line_start,
               line_end, signature, doc, unit, is_test, chunk_key, tagged, text, name_line,
               name_column)
             VALUES (9999, 'ghost.rs', 'rust', 'function', 'ghost', 'ghost', 1, 1, 'fn ghost()',
               '', NULL, 0, zeroblob(32), 1, 'fn ghost() {}', 1, 4)",
            [],
        )
        .unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_removed, 1);
    assert_eq!(stored_rows(&w.dir, "ghost.rs"), 0);
}

/// A rebuild whose last vector checkpoint cannot be written is not recorded as finished.
#[test]
fn a_rebuild_is_finished_only_once_its_vectors_are_saved() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    // A directory where the checkpoint goes: saving it fails.
    let vectors = w.dir.join("vectors.usearch");
    fs::remove_file(&vectors).unwrap();
    fs::create_dir(&vectors).unwrap();
    let forced = IndexOptions {
        force: true,
        paths: None,
    };
    assert!(e.index_blocking(&w, forced).is_err());
    let manifest = lodestone::index::Manifest::load(&w.dir.join("manifest.json"))
        .unwrap()
        .unwrap();
    assert!(manifest.rebuilding);
    fs::remove_dir(&vectors).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let manifest = lodestone::index::Manifest::load(&w.dir.join("manifest.json"))
        .unwrap()
        .unwrap();
    assert!(!manifest.rebuilding);
}

/// A file unreadable from the start, then deleted, is no longer reported once a watcher pass
/// sees it gone.
#[cfg(unix)]
#[test]
fn a_never_indexed_unreadable_file_is_forgotten_once_deleted() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let repo = root.to_string_lossy().to_string();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let file = root.join("core/src/new.rs");
    write(&root, "core/src/new.rs", "pub fn brand_new() {}\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let scoped = || IndexOptions {
        force: false,
        paths: Some(vec!["core/src/new.rs".into()]),
    };
    e.index_blocking(&w, scoped()).unwrap();
    assert!(e.status(Some(&repo)).unwrap().stale);
    fs::remove_file(&file).unwrap();
    e.index_blocking(&w, scoped()).unwrap();
    assert!(!e.status(Some(&repo)).unwrap().stale);
}

/// A rebuild covers the whole tree, even when asked for some paths.
#[test]
fn a_rebuild_ignores_a_path_scope() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let items = w.snapshot().items.len();
    e.index_blocking(
        &w,
        IndexOptions {
            force: true,
            paths: Some(vec!["core/src/lib.rs".into()]),
        },
    )
    .unwrap();
    assert_eq!(w.snapshot().items.len(), items);
}

/// A chunking change while a subtree is unreadable re-chunks its files once they can be read.
#[cfg(unix)]
#[test]
fn files_missed_during_a_chunking_change_are_rechunked_later() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    let web = root.join("web");
    fs::set_permissions(&web, fs::Permissions::from_mode(0o000)).unwrap();
    let mut config = Config {
        data_dir: Some(data.clone()),
        chunk_tokens: 50,
        ..Config::default()
    };
    config.rerank.enabled = false;
    let e = Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap();
    let w = e.register(&root).unwrap();
    let missed = e.index_blocking(&w, IndexOptions::default());
    fs::set_permissions(&web, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(missed.is_ok());
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, 1, "web/ui.ts, re-chunked");
}

/// `from_unit` outside Rust: in a pnpm workspace, `@acme/ui` sees itself and `@acme/core`, not
/// the app that depends on it.
#[test]
fn from_unit_scopes_a_js_workspace() {
    let root = tempdir().join("repo");
    write(&root, "pnpm-workspace.yaml", "packages:\n  - packages/*\n");
    write(
        &root,
        "packages/core/package.json",
        r#"{"name": "@acme/core"}"#,
    );
    write(
        &root,
        "packages/ui/package.json",
        r#"{"name": "@acme/ui", "dependencies": {"@acme/core": "workspace:*"}}"#,
    );
    write(
        &root,
        "packages/app/package.json",
        r#"{"name": "@acme/app", "dependencies": {"@acme/ui": "workspace:*"}}"#,
    );
    write(
        &root,
        "packages/core/src/date.ts",
        "/** Format a date for display. */\nexport function formatDate() {}\n",
    );
    write(
        &root,
        "packages/ui/src/label.ts",
        "/** Render a label showing a formatted date. */\nexport function renderDateLabel() {}\n",
    );
    write(
        &root,
        "packages/app/src/main.ts",
        "/** Start the app and format today's date. */\nexport function startApp() {}\n",
    );
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let resp = e
        .search(&SearchRequest {
            query: "format a date".into(),
            repo: Some(root.to_string_lossy().to_string()),
            from_unit: Some("@acme/ui".into()),
            top_k: 50,
            ..Default::default()
        })
        .unwrap();
    let names: BTreeSet<&str> = resp.results.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["formatDate", "renderDateLabel"]),
        "{:?}",
        resp.notes
    );
    let units: BTreeSet<Option<&str>> = resp.results.iter().map(|r| r.unit.as_deref()).collect();
    assert_eq!(
        units,
        BTreeSet::from([Some("@acme/core"), Some("@acme/ui")])
    );
}

/// Units are checked against the build graph on the first pass, whatever graph the index was
/// opened with (here none: `graph.json` is gone, and the graph built on opening is the new one).
#[test]
fn units_are_reconciled_when_the_graph_checkpoint_is_lost() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let dir = w.dir.clone();
    drop((e, w));
    write(&root, "core/Cargo.toml", "[package]\nname = \"kernel\"\n");
    fs::remove_file(dir.join("graph.json")).unwrap();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let resp = e
        .search(&SearchRequest {
            query: "retry_with_backoff".into(),
            repo: Some(root.to_string_lossy().to_string()),
            from_unit: Some("kernel".into()),
            top_k: 8,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(resp.results[0].name, "retry_with_backoff");
}

/// A cached vector that cannot be used (cached before vectors were checked) is re-embedded and
/// replaced, not reused.
#[test]
fn unusable_cached_vectors_are_replaced() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let (entries, _) = e.cache.stats().unwrap();
    rusqlite::Connection::open(data.join("cache.sqlite"))
        .unwrap()
        .execute("UPDATE embeddings SET vector = x''", [])
        .unwrap();
    let r = e
        .index_blocking(
            &w,
            IndexOptions {
                force: true,
                paths: None,
            },
        )
        .unwrap();
    assert_eq!((r.chunks_cached, r.chunks_embedded > 0), (0, true));
    assert_eq!(e.cache.stats().unwrap(), (entries, entries * 64 * 4));
}

/// A watcher event for a file that was never indexed removes nothing and commits nothing.
#[test]
fn an_unindexed_path_is_not_removed() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let version = w.snapshot().version;
    write(&root, "build.out", "artifact\n");
    let r = e
        .index_blocking(
            &w,
            IndexOptions {
                force: false,
                paths: Some(vec!["build.out".into(), "gone.rs".into()]),
            },
        )
        .unwrap();
    assert_eq!(
        (r.files_scanned, r.files_changed, r.files_removed),
        (0, 0, 0)
    );
    // Only the end-of-pass publication, no batch commit.
    assert_eq!(w.snapshot().version, version + 1);
}

/// Renaming a package moves its items to the new unit without any source file changing.
#[test]
fn a_renamed_package_moves_its_items() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    write(&root, "core/Cargo.toml", "[package]\nname = \"kernel\"\n");
    write(
        &root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\n[dependencies]\nkernel = { path = \"../core\" }\n",
    );
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.files_changed, 0, "no source file changed");
    let resp = e
        .search(&SearchRequest {
            query: "retry_with_backoff".into(),
            repo: Some(root.to_string_lossy().to_string()),
            from_unit: Some("kernel".into()),
            top_k: 8,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(resp.results[0].name, "retry_with_backoff");
    assert_eq!(resp.results[0].unit.as_deref(), Some("kernel"));
    // The change is stored, not only published.
    drop((e, w));
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    assert!(
        w.snapshot()
            .items
            .values()
            .filter(|r| r.item.rel_path.starts_with("core/"))
            .all(|r| r.unit.as_deref() == Some("kernel"))
    );
}

/// Without query vectors (an index built with another model), a search falls back to lexical
/// results even though the default config weights them 0.
#[test]
fn a_model_mismatch_falls_back_to_lexical_results() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    drop((e, w));
    let mut config = Config {
        data_dir: Some(data.clone()),
        ..Config::default()
    };
    config.rerank.enabled = false;
    let other = HashEmbedder {
        dim: 64,
        id: "test/other".into(),
        delay_ms: 0,
    };
    let e = Engine::with_embedder(config, Arc::new(other)).unwrap();
    let resp = search(&e, &root, "retry_with_backoff");
    assert!(
        resp.notes
            .iter()
            .any(|n| n.contains("lexical results only"))
    );
    assert_eq!(resp.results[0].name, "retry_with_backoff");
}

/// `cache_max_gb` bounds the embedding cache after indexing.
#[test]
fn indexing_prunes_the_cache_to_its_budget() {
    let root = fixture();
    let data = tempdir();
    let mut config = Config {
        data_dir: Some(data.clone()),
        cache_max_gb: 0.0,
        ..Config::default()
    };
    config.rerank.enabled = false;
    let e = Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap();
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(r.chunks_embedded > 0);
    assert_eq!(e.cache.stats().unwrap(), (0, 0));
    assert!(!search(&e, &root, "retry_with_backoff").results.is_empty());
}

/// A lowered budget applies on the first pass, though nothing is embedded.
#[test]
fn a_lowered_cache_budget_applies_without_new_embeddings() {
    let root = fixture();
    let data = tempdir();
    let (e, _) = engine(&data);
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(e.cache.stats().unwrap().0 > 0);
    drop((e, w));
    let mut config = Config {
        data_dir: Some(data.clone()),
        cache_max_gb: 0.0,
        ..Config::default()
    };
    config.rerank.enabled = false;
    let e = Engine::with_embedder(config, Arc::new(hash_embedder())).unwrap();
    let w = e.register(&root).unwrap();
    let r = e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert_eq!(r.chunks_embedded, 0);
    assert_eq!(e.cache.stats().unwrap(), (0, 0));
}

/// A registry that cannot be read is reported, never overwritten.
#[test]
fn an_unreadable_registry_is_not_overwritten() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let registry = data.join("registry.json");
    fs::write(&registry, "{ not json").unwrap();
    let err = e.register(&fixture()).err().unwrap();
    assert!(
        format!("{err:#}").contains("not a valid registry"),
        "{err:#}"
    );
    assert_eq!(fs::read_to_string(&registry).unwrap(), "{ not json");
}

/// A worktree is unregistered by name or by a path in it, even once deleted, and its index goes
/// with it; other worktrees are untouched.
#[test]
fn unregister_drops_the_worktree_and_its_index() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let (a, b, c) = (fixture(), fixture(), fixture());
    let indexed = |root: &Path| {
        let w = e.register(root).unwrap();
        e.index_blocking(&w, IndexOptions::default()).unwrap();
        let dir = data.join("worktrees").join(&w.id);
        assert!(dir.exists());
        (e.repo_name(&w.root), w.root.clone(), dir)
    };
    let (a_name, a_root, a_dir) = indexed(&a);
    let (_, b_root, b_dir) = indexed(&b);
    let (_, c_root, c_dir) = indexed(&c);
    let roots = || -> Vec<String> { e.repos().into_iter().map(|r| r.root).collect() };

    let info = e.unregister(Some(&a_name), None).unwrap();
    assert_eq!(
        (info.name, Path::new(&info.root)),
        (a_name, a_root.as_path())
    );
    assert!(!a_dir.exists());
    assert!(b_dir.exists() && c_dir.exists());
    assert_eq!(roots().len(), 2);

    e.unregister(Some(&b.join("core/src").to_string_lossy()), None)
        .unwrap();
    assert!(!b_dir.exists());

    fs::remove_dir_all(&c).unwrap();
    e.unregister(Some(&c_root.to_string_lossy()), None).unwrap();
    assert!(!c_dir.exists());
    assert!(roots().is_empty());

    let err = e
        .unregister(Some(&b_root.to_string_lossy()), None)
        .err()
        .unwrap();
    assert!(
        format!("{err:#}").contains("not a registered worktree"),
        "{err:#}"
    );
}

/// Unregistering stops a running pass at its next batch, and no later pass writes the deleted
/// index back.
#[test]
fn unregister_stops_indexing_and_leaves_no_index_behind() {
    let root = fixture();
    for i in 0..300 {
        write(
            &root,
            &format!("core/src/gen{i}.rs"),
            &format!("/// Generated helper {i}.\npub fn generated_{i}() {{}}\n"),
        );
    }
    let data = tempdir();
    let (e, emb) = engine(&data);
    let e = Arc::new(e);
    let w = e.register(&root).unwrap();
    emb.slow.store(true, Ordering::SeqCst);
    let pass = {
        let (e, w) = (e.clone(), w.clone());
        std::thread::spawn(move || e.index_blocking(&w, IndexOptions::default()))
    };
    while !w.is_writing() {
        std::thread::yield_now();
    }
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    let err = pass.join().unwrap().expect_err("the pass stops");
    assert!(format!("{err:#}").contains("unregistered"), "{err:#}");
    assert!(!w.dir.exists());
    assert!(e.index_blocking(&w, IndexOptions::default()).is_err());
    assert!(!w.dir.exists());
}

/// Sibling worktrees `alpha`, `bravo` and `gone` (canonical) in one directory, each registered.
fn siblings(e: &Engine) -> (PathBuf, PathBuf, PathBuf) {
    let base = tempdir().canonicalize().unwrap();
    let roots = ["alpha", "bravo", "gone"].map(|n| {
        let root = base.join(n);
        write(&root, "lib.rs", "pub fn f() {}\n");
        e.register(&root).unwrap();
        root
    });
    let [a, b, g] = roots;
    (a, b, g)
}

/// A name no longer registered, or a missing path, never falls back to the worktree it would be
/// inside; a missing path names the registered root it resolves to.
#[test]
fn unregister_matches_a_missing_path_only_to_the_root_it_names() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let (alpha, _bravo, gone) = siblings(&e);
    let roots = || -> Vec<String> { e.repos().into_iter().map(|r| r.root).collect() };

    // `lode rm bravo` run twice from `alpha`: the second names the missing `alpha/bravo`.
    e.unregister(Some("bravo"), None).unwrap();
    let err = e
        .unregister(Some(&alpha.join("bravo").to_string_lossy()), None)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a registered worktree"),
        "{err:#}"
    );
    assert_eq!(roots().len(), 2);

    // `lode rm ../gone` from `alpha`, once `gone` is deleted.
    fs::remove_dir_all(&gone).unwrap();
    let info = e
        .unregister(Some(&alpha.join("../gone").to_string_lossy()), None)
        .unwrap();
    assert_eq!(Path::new(&info.root), gone);
    assert_eq!(roots(), [alpha.to_string_lossy()]);
}

/// A repository's main checkout and a linked worktree of it (as `git worktree add` leaves them),
/// canonical, each with a source file.
fn linked_worktree() -> (PathBuf, PathBuf) {
    let base = tempdir().canonicalize().unwrap();
    let (main, wt) = (base.join("main"), base.join("wt"));
    let git_dir = main.join(".git/worktrees/wt");
    write(&git_dir, "commondir", "../..\n");
    write(&wt, ".git", &format!("gitdir: {}\n", git_dir.display()));
    write(&main, "main.rs", "pub fn in_main() {}\n");
    write(&wt, "wt.rs", "pub fn in_wt() {}\n");
    (main, wt)
}

/// A registered root inside a linked git worktree is removed itself, not the worktree around it
/// that requests in it are routed to.
#[test]
fn unregister_takes_an_exact_registered_root_inside_a_linked_worktree() {
    let (_, wt) = linked_worktree();
    write(&wt, "package/lib.rs", "pub fn f() {}\n");
    let data = tempdir();
    let (e, _) = engine(&data);
    let package = wt.join("package");
    let (w, p) = (e.register(&wt).unwrap(), e.register(&package).unwrap());
    let info = e
        .unregister(Some(&package.to_string_lossy()), None)
        .unwrap();
    assert_eq!(Path::new(&info.root), package);
    assert_eq!(
        e.repos().into_iter().map(|r| r.root).collect::<Vec<_>>(),
        [wt.to_string_lossy()]
    );
    assert!(w.dir.exists() && !p.dir.exists());
}

/// An index another process is writing is neither deleted nor unregistered, and that process's
/// pass is unaffected.
#[test]
fn unregister_leaves_an_index_another_process_is_writing() {
    let root = fixture();
    for i in 0..300 {
        write(
            &root,
            &format!("core/src/gen{i}.rs"),
            &format!("/// Generated helper {i}.\npub fn generated_{i}() {{}}\n"),
        );
    }
    let data = tempdir();
    // Two engines over one data directory, as two processes.
    let ((writer, emb), (remover, _)) = (engine(&data), engine(&data));
    let writer = Arc::new(writer);
    let w = writer.register(&root).unwrap();
    emb.slow.store(true, Ordering::SeqCst);
    let pass = {
        let (e, w) = (writer.clone(), w.clone());
        std::thread::spawn(move || e.index_blocking(&w, IndexOptions::default()))
    };
    while !w.is_writing() {
        std::thread::yield_now();
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let err = remover
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("another lodestone process"),
        "{err:#}"
    );
    assert_eq!(remover.repos().len(), 1);
    pass.join().unwrap().unwrap();
    assert!(w.dir.exists());
    // Once that process has exited.
    let dir = w.dir.clone();
    drop((writer, w));
    remover
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap();
    assert!(!dir.exists());
}

/// An index that cannot be deleted is reported and stays registered, so removal can be retried;
/// it is left either intact or gone, never half-deleted.
#[cfg(unix)]
#[test]
fn a_failed_deletion_keeps_the_registration() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fixture();
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    let chmod = |p: &Path, m| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    let unregister = || e.unregister(Some(&root.to_string_lossy()), None);

    // It cannot be moved aside: intact.
    chmod(&data.join("worktrees"), 0o555);
    let err = unregister().unwrap_err();
    chmod(&data.join("worktrees"), 0o755);
    assert!(format!("{err:#}").contains("deleting"), "{err:#}");
    assert_eq!(e.repos().len(), 1);
    assert!(!search(&e, &root, "retry with backoff").results.is_empty());

    // Moved aside, but not deleted: gone, its leftovers removed by the retry.
    chmod(&w.dir, 0o555);
    let err = unregister().unwrap_err();
    let aside = w.dir.with_extension("deleting");
    chmod(&aside, 0o755);
    assert!(format!("{err:#}").contains("deleting"), "{err:#}");
    assert_eq!(e.repos().len(), 1);
    assert!(!w.dir.exists());
    unregister().unwrap();
    assert!(e.repos().is_empty() && !aside.exists());
}

/// A request that looked a worktree up before it was unregistered does not reopen its index.
#[test]
fn an_unregistered_index_is_not_reopened() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fs::canonicalize(fixture()).unwrap();
    let w = e.register(&root).unwrap();
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    let err = e.open_canonical(&root).err().unwrap();
    assert!(
        format!("{err:#}").contains("not a registered worktree"),
        "{err:#}"
    );
    assert!(!w.dir.exists());
}

/// A worktree registered again right after it was unregistered keeps the watch started for it;
/// only the retired index's watch is dropped.
#[test]
fn a_new_registration_keeps_its_watch_through_the_old_ones_teardown() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    let data = tempdir();
    let (e, _) = engine(&data);
    let e = Arc::new(e);
    let root = fs::canonicalize(fixture()).unwrap();
    let watchers = Watchers::default();
    watchers.watch(&e, e.register(&root).unwrap());
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    assert!(!watchers.is_watched(&root));
    let w = e.register(&root).unwrap();
    watchers.watch(&e, w.clone());
    watchers.unwatch_retired();
    assert!(watchers.is_watched(&root) && !w.is_retired());
}

/// A process that had the index open while another deleted it (`lode --local rm` beside a
/// daemon) reopens it once registered again, instead of writing to the deleted directory.
#[test]
#[cfg_attr(
    windows,
    ignore = "Windows cannot delete an index another process has open"
)]
fn an_index_deleted_by_another_process_is_reopened() {
    let data = tempdir();
    let ((daemon, _), (local, _)) = (engine(&data), engine(&data));
    let root = fs::canonicalize(fixture()).unwrap();
    let old = daemon.register(&root).unwrap();
    daemon
        .index_blocking(&old, IndexOptions::default())
        .unwrap();
    old.release_writer();
    local
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap();
    let new = daemon.register(&root).unwrap();
    assert!(!Arc::ptr_eq(&old, &new) && old.is_retired());
    daemon
        .index_blocking(&new, IndexOptions::default())
        .unwrap();
    assert!(
        !search(&daemon, &root, "retry with backoff")
            .results
            .is_empty()
    );
}

/// Registry updates from several processes at once all land.
#[test]
fn concurrent_processes_unregister_without_losing_updates() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let base = tempdir().canonicalize().unwrap();
    let roots: Vec<PathBuf> = (0..12)
        .map(|i| {
            let root = base.join(format!("r{i}"));
            write(&root, "lib.rs", "pub fn f() {}\n");
            e.register(&root).unwrap();
            root
        })
        .collect();
    let removers: Vec<_> = roots
        .into_iter()
        .map(|root| {
            let data = data.clone();
            std::thread::spawn(move || {
                engine(&data)
                    .0
                    .unregister(Some(&root.to_string_lossy()), None)
            })
        })
        .collect();
    for r in removers {
        r.join().unwrap().unwrap();
    }
    assert!(e.repos().is_empty(), "{:?}", e.repos());
}

/// An index that cannot even be looked at is not taken for one already gone.
#[cfg(unix)]
#[test]
fn an_inaccessible_index_is_not_taken_for_a_missing_one() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fixture();
    let w = e.register(&root).unwrap();
    let worktrees = data.join("worktrees");
    let mode = |m| fs::set_permissions(&worktrees, fs::Permissions::from_mode(m)).unwrap();
    mode(0o600);
    let err = e
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap_err();
    mode(0o755);
    assert!(format!("{err:#}").contains("deleting"), "{err:#}");
    assert_eq!(e.repos().len(), 1);
    assert!(w.dir.exists());
}

/// An index directory that is a symlink is removed as the link, never what it points to.
#[cfg(unix)]
#[test]
fn a_symlinked_index_is_removed_as_the_link() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fixture();
    let w = e.register(&root).unwrap();
    let elsewhere = tempdir();
    write(&elsewhere, "keep.txt", "unrelated\n");
    fs::remove_dir_all(&w.dir).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &w.dir).unwrap();
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    assert!(fs::symlink_metadata(&w.dir).is_err());
    assert!(elsewhere.join("keep.txt").exists());
}

/// A removed linked worktree whose index cannot be deleted yet stays registered, and is pruned
/// once it can be.
#[test]
fn a_failed_prune_is_retried() {
    let (main, wt) = linked_worktree();
    let data = tempdir();
    let ((e, _), (other, _)) = (engine(&data), engine(&data));
    e.register(&main).unwrap();
    e.worktree_for(&wt).unwrap();
    // Another process holds the worktree's writer lock.
    let held = other.worktree_for(&wt).unwrap();
    other
        .index_blocking(&held, IndexOptions::default())
        .unwrap();
    fs::remove_dir_all(&wt).unwrap();
    assert!(e.tidy_worktrees().unwrap().is_empty());
    assert_eq!(e.repos().len(), 2);
    // Once that process has exited.
    let dir = held.dir.clone();
    drop((other, held));
    assert_eq!(e.tidy_worktrees().unwrap(), [wt]);
    assert_eq!(e.repos().len(), 1);
    assert!(!dir.exists());
}

/// Result names for `query` in `root`.
fn found(e: &Engine, root: &Path, query: &str) -> BTreeSet<String> {
    search(e, root, query)
        .results
        .into_iter()
        .map(|r| r.name)
        .collect()
}

/// An index deleted and created again by another process (`--local rm`, then `--local register`)
/// is told apart from the one a process had open, which it then reopens.
#[test]
#[cfg_attr(
    windows,
    ignore = "Windows cannot delete an index another process has open"
)]
fn an_index_created_again_by_another_process_is_reopened() {
    let data = tempdir();
    let ((daemon, _), (local, _)) = (engine(&data), engine(&data));
    let root = fs::canonicalize(fixture()).unwrap();
    let old = daemon.register(&root).unwrap();
    daemon
        .index_blocking(&old, IndexOptions::default())
        .unwrap();
    old.release_writer();
    local
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap();
    fs::remove_file(root.join("core/src/lib.rs")).unwrap();
    write(
        &root,
        "core/src/new.rs",
        "/// Freshly added.\npub fn freshly_added() {}\n",
    );
    let w = local.register(&root).unwrap();
    local.index_blocking(&w, IndexOptions::default()).unwrap();

    let names = found(&daemon, &root, "freshly_added retry_with_backoff");
    assert!(names.contains("freshly_added"), "{names:?}");
    assert!(!names.contains("retry_with_backoff"), "{names:?}");
    assert!(old.is_retired());
    let err = daemon
        .index_blocking(&old, IndexOptions::default())
        .unwrap_err();
    assert!(format!("{err:#}").contains("unregistered"), "{err:#}");
}

/// A watch whose worktree another process unregisters ends at its next pass, without writing
/// the deleted index back.
#[test]
#[cfg_attr(
    windows,
    ignore = "Windows cannot delete an index another process has open"
)]
fn a_watch_ends_when_another_process_unregisters_its_worktree() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    let data = tempdir();
    let ((daemon, _), (local, _)) = (engine(&data), engine(&data));
    let daemon = Arc::new(daemon);
    let root = fs::canonicalize(fixture()).unwrap();
    let w = daemon.register(&root).unwrap();
    let watchers = Watchers::default();
    watchers.spawn_index(&daemon, w.clone(), IndexOptions::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while w.manifest().is_none() || w.is_writing() {
        assert!(std::time::Instant::now() < deadline, "first index");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    local
        .unregister(Some(&root.to_string_lossy()), None)
        .unwrap();
    write(&root, "core/src/edited.rs", "pub fn edited() {}\n");
    while watchers.watched().contains(&root) {
        assert!(std::time::Instant::now() < deadline, "the watch to end");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(w.is_retired() && !w.dir.exists());
}

/// Dropping the watchers ends their workers, which release their indexes.
#[test]
fn dropped_watchers_end_their_workers() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    let data = tempdir();
    let e = Arc::new(engine(&data).0);
    let root = fs::canonicalize(fixture()).unwrap();
    let w = e.register(&root).unwrap();
    let unwatched = Arc::strong_count(&w);
    let watchers = Watchers::default();
    watchers.watch(&e, w.clone());
    assert!(Arc::strong_count(&w) > unwatched);
    drop(watchers);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while Arc::strong_count(&w) > unwatched {
        assert!(std::time::Instant::now() < deadline, "the worker to end");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The writer lock file outlives removal, so a writer that opened it before and one after a
/// removal and registration again lock the same file.
#[test]
fn the_writer_lock_file_survives_removal() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fs::canonicalize(fixture()).unwrap();
    let w = e.register(&root).unwrap();
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    w.release_writer();
    let lock_path = w.dir.with_extension("lock");
    let before = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    assert!(lock_path.exists());
    let again = e.register(&root).unwrap();
    // Keeps its writer, and so the lock, after the pass.
    e.index_blocking(&again, IndexOptions::default()).unwrap();
    std::assert_matches!(before.try_lock(), Err(fs::TryLockError::WouldBlock));
}

/// Marks `root`'s entry in the registry under `data` as being removed, as a removal that
/// stopped part-way leaves it.
fn mark_removing(data: &Path, root: &Path) {
    let path = data.join("registry.json");
    let mut reg: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for r in reg["repos"].as_array_mut().unwrap() {
        if Path::new(r["root"].as_str().unwrap()) == root {
            r["removing"] = true.into();
        }
    }
    fs::write(&path, serde_json::to_vec(&reg).unwrap()).unwrap();
}

/// A removal that stopped part-way (here, once the index was moved aside) is no longer served,
/// and is finished by the next attempt or the daemon's tidying.
#[test]
fn an_interrupted_removal_is_finished_later() {
    let data = tempdir();
    let (e, _) = engine(&data);
    // Indexed by another process, which then stopped part-way through removing it.
    let interrupted = || {
        let root = fs::canonicalize(fixture()).unwrap();
        let (other, _) = engine(&data);
        let w = other.register(&root).unwrap();
        other.index_blocking(&w, IndexOptions::default()).unwrap();
        let dir = w.dir.clone();
        drop((other, w));
        let aside = dir.with_extension("deleting");
        fs::rename(&dir, &aside).unwrap();
        mark_removing(&data, &root);
        (root, aside)
    };

    let (root, aside) = interrupted();
    assert!(e.repos().is_empty());
    assert!(e.resolve(Some(&root.to_string_lossy())).is_err());
    e.unregister(Some(&root.to_string_lossy()), None).unwrap();
    assert!(!aside.exists());

    let (root, aside) = interrupted();
    assert_eq!(e.tidy_worktrees().unwrap(), [root]);
    assert!(!aside.exists());
    let raw = fs::read_to_string(data.join("registry.json")).unwrap();
    assert!(!raw.contains("repo\""), "{raw}");

    // Registering it again finishes the removal first.
    let (root, aside) = interrupted();
    let w = e.register(&root).unwrap();
    assert!(!aside.exists());
    assert_eq!(e.repos().len(), 1);
    e.index_blocking(&w, IndexOptions::default()).unwrap();
    assert!(!search(&e, &root, "a function").results.is_empty());
}

/// An index is not created again by a process that looked its worktree up while another was
/// removing it.
#[test]
fn a_load_waits_for_a_removal_in_another_process() {
    let data = tempdir();
    let (e, _) = engine(&data);
    let root = fs::canonicalize(fixture()).unwrap();
    let dir = e.register(&root).unwrap().dir.clone();
    let (fresh, _) = engine(&data);
    // Another process removing it: holds the registry lock until the entry and index are gone.
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(data.join("registry.lock"))
        .unwrap();
    lock.lock().unwrap();
    let opening = {
        let root = root.clone();
        std::thread::spawn(move || fresh.open_canonical(&root).map(|_| ()))
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!opening.is_finished());
    fs::write(data.join("registry.json"), "{\"repos\": []}").unwrap();
    fs::remove_dir_all(&dir).unwrap();
    drop(lock);
    let err = opening.join().unwrap().unwrap_err();
    assert!(
        format!("{err:#}").contains("not a registered worktree"),
        "{err:#}"
    );
    assert!(!dir.exists());
}
