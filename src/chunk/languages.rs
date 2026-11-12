//! Language registry: compiled-in grammars plus grammars discovered through tree-sitter's own
//! config. A language is a grammar, its tags query (merged with lodestone's supplementary
//! overlay) and an optional tests query. Component formats (see [`super::component`]) are
//! registered alongside. Nothing here knows about any specific language.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tree_sitter::{Language as Grammar, Query};
use tree_sitter_tags::TagsConfiguration;

use super::component::Component;
use crate::config::LanguagesConfig;

/// Where a language came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Builtin,
    External(PathBuf),
}

pub struct Language {
    pub name: String,
    pub grammar: Grammar,
    pub tags: TagsConfiguration,
    pub tests: Option<Query>,
    pub extensions: Vec<String>,
    pub origin: Origin,
    /// Hash of the query texts, so index-time chunking changes are detectable.
    pub query_hash: String,
}

impl std::fmt::Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Language")
            .field("name", &self.name)
            .field("extensions", &self.extensions)
            .field("origin", &self.origin)
            .finish()
    }
}

/// A grammar compiled into the binary behind a `lang-*` feature.
struct Builtin {
    name: &'static str,
    grammar: Grammar,
    tags: Vec<&'static str>,
    locals: &'static str,
    extensions: &'static [&'static str],
}

/// Supplementary queries shipped with lodestone, keyed by language name and query kind.
const OVERLAYS: &[(&str, &str, &str)] = &[
    ("rust", "tags", include_str!("../../queries/rust/tags.scm")),
    (
        "rust",
        "tests",
        include_str!("../../queries/rust/tests.scm"),
    ),
    (
        "python",
        "tags",
        include_str!("../../queries/python/tags.scm"),
    ),
    (
        "python",
        "tests",
        include_str!("../../queries/python/tests.scm"),
    ),
    (
        "markdown",
        "tags",
        include_str!("../../queries/markdown/tags.scm"),
    ),
    (
        "typescript",
        "tags",
        include_str!("../../queries/typescript/tags.scm"),
    ),
    (
        "typescript",
        "tests",
        include_str!("../../queries/typescript/tests.scm"),
    ),
    (
        "tsx",
        "tags",
        include_str!("../../queries/typescript/tags.scm"),
    ),
    (
        "tsx",
        "tests",
        include_str!("../../queries/typescript/tests.scm"),
    ),
    (
        "javascript",
        "tests",
        include_str!("../../queries/javascript/tests.scm"),
    ),
    ("go", "tests", include_str!("../../queries/go/tests.scm")),
    (
        "clarity",
        "tags",
        include_str!("../../queries/clarity/tags.scm"),
    ),
];

#[allow(unused_mut, clippy::vec_init_then_push)]
fn builtins() -> Vec<Builtin> {
    let mut v = Vec::new();
    #[cfg(feature = "lang-rust")]
    v.push(Builtin {
        name: "rust",
        grammar: tree_sitter_rust::LANGUAGE.into(),
        tags: vec![tree_sitter_rust::TAGS_QUERY],
        locals: "",
        extensions: &["rs"],
    });
    #[cfg(feature = "lang-python")]
    v.push(Builtin {
        name: "python",
        grammar: tree_sitter_python::LANGUAGE.into(),
        tags: vec![tree_sitter_python::TAGS_QUERY],
        locals: "",
        extensions: &["py", "pyi"],
    });
    #[cfg(feature = "lang-typescript")]
    {
        v.push(Builtin {
            name: "javascript",
            grammar: tree_sitter_javascript::LANGUAGE.into(),
            tags: vec![tree_sitter_javascript::TAGS_QUERY],
            locals: tree_sitter_javascript::LOCALS_QUERY,
            extensions: &["js", "mjs", "cjs", "jsx"],
        });
        // TypeScript's tags.scm only covers TS-specific nodes; it is designed to be combined
        // with JavaScript's.
        v.push(Builtin {
            name: "typescript",
            grammar: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tags: vec![
                tree_sitter_typescript::TAGS_QUERY,
                tree_sitter_javascript::TAGS_QUERY,
            ],
            locals: tree_sitter_typescript::LOCALS_QUERY,
            extensions: &["ts", "mts", "cts"],
        });
        v.push(Builtin {
            name: "tsx",
            grammar: tree_sitter_typescript::LANGUAGE_TSX.into(),
            tags: vec![
                tree_sitter_typescript::TAGS_QUERY,
                tree_sitter_javascript::TAGS_QUERY,
            ],
            locals: tree_sitter_typescript::LOCALS_QUERY,
            extensions: &["tsx"],
        });
    }
    #[cfg(feature = "lang-go")]
    v.push(Builtin {
        name: "go",
        grammar: tree_sitter_go::LANGUAGE.into(),
        tags: vec![tree_sitter_go::TAGS_QUERY],
        locals: "",
        extensions: &["go"],
    });
    #[cfg(feature = "lang-markdown")]
    v.push(Builtin {
        name: "markdown",
        grammar: tree_sitter_md::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["md", "markdown"],
    });
    #[cfg(feature = "lang-clarity")]
    v.push(Builtin {
        name: "clarity",
        grammar: super::clarity::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["clar"],
    });
    v
}

#[allow(unused_mut, clippy::vec_init_then_push)]
fn components() -> Vec<Component> {
    let mut v = Vec::new();
    #[cfg(feature = "lang-svelte")]
    v.push(Component {
        name: "svelte",
        extensions: &["svelte"],
        grammar: tree_sitter_svelte_ng::LANGUAGE.into(),
    });
    v
}

/// Summary row for `lode languages`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LanguageInfo {
    pub name: String,
    pub origin: String,
    pub extensions: Vec<String>,
    pub has_tests_query: bool,
}

#[derive(Default)]
pub struct LanguageRegistry {
    languages: Vec<Arc<Language>>,
    by_extension: HashMap<String, usize>,
    components: Vec<Arc<Component>>,
    text_extensions: HashSet<String>,
    /// Grammars that were found but could not be used (ABI mismatch, bad query, no tags).
    pub issues: Vec<String>,
}

impl LanguageRegistry {
    /// Builds the registry from built-in grammars, user overlays under `<data_dir>/queries`,
    /// and grammars found via tree-sitter's config. Failures are recorded in `issues`.
    pub fn load(config: &LanguagesConfig, data_dir: &Path) -> Self {
        let mut reg = Self {
            text_extensions: config
                .index_untagged_as_text
                .iter()
                .map(|e| e.to_lowercase())
                .collect(),
            ..Self::default()
        };
        let user_overlays = data_dir.join("queries");
        for b in builtins() {
            let mut tags_parts = vec![overlay(&user_overlays, b.name, "tags")];
            tags_parts.extend(b.tags.iter().map(|s| s.to_string()));
            let tests = overlay(&user_overlays, b.name, "tests");
            match build_language(
                b.name,
                b.grammar,
                &tags_parts.join("\n"),
                b.locals,
                &tests,
                b.extensions.iter().map(|s| s.to_string()).collect(),
                Origin::Builtin,
            ) {
                Ok(lang) => reg.add(lang),
                Err(e) => reg.issues.push(format!("{}: {e}", b.name)),
            }
        }
        reg.components = components().into_iter().map(Arc::new).collect();
        if let Some(ts_config) = &config.tree_sitter_config {
            reg.load_external(&expand_home(ts_config), data_dir, &user_overlays);
        }
        for (ext, lang) in &config.extensions {
            if let Some(idx) = reg.languages.iter().position(|l| &l.name == lang) {
                reg.by_extension.insert(ext.to_lowercase(), idx);
            } else {
                reg.issues
                    .push(format!("extension .{ext} maps to unknown language {lang}"));
            }
        }
        reg
    }

    fn add(&mut self, lang: Language) {
        let idx = self.languages.len();
        for ext in &lang.extensions {
            self.by_extension.entry(ext.to_lowercase()).or_insert(idx);
        }
        self.languages.push(Arc::new(lang));
    }

    fn load_external(&mut self, ts_config: &Path, data_dir: &Path, user_overlays: &Path) {
        let Ok(text) = fs::read_to_string(ts_config) else {
            return;
        };
        let parsed: tree_sitter_loader::Config = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                self.issues.push(format!(
                    "{}: invalid tree-sitter config: {e}",
                    ts_config.display()
                ));
                return;
            }
        };
        let mut loader =
            tree_sitter_loader::Loader::with_parser_lib_path(data_dir.join("grammars"));
        if let Err(e) = loader.find_all_languages(&parsed) {
            self.issues.push(format!("tree-sitter loader: {e}"));
            return;
        }
        let known: HashSet<String> = self.languages.iter().map(|l| l.name.clone()).collect();
        for (conf, _) in loader.get_all_language_configurations() {
            let name = conf.language_name.clone();
            if known.contains(&name) || self.languages.iter().any(|l| l.name == name) {
                continue;
            }
            let grammar = match loader.language_for_configuration(conf) {
                Ok(g) => g,
                Err(e) => {
                    self.issues
                        .push(format!("{name}: cannot load grammar: {e}"));
                    continue;
                }
            };
            let abi = grammar.abi_version();
            if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
                .contains(&abi)
            {
                self.issues.push(format!(
                    "{name}: grammar ABI {abi} outside supported range {}..={}",
                    tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
                    tree_sitter::LANGUAGE_VERSION
                ));
                continue;
            }
            let read_all = |files: &Option<Vec<PathBuf>>, default: &str| -> String {
                let paths = files
                    .clone()
                    .unwrap_or_else(|| vec![PathBuf::from("queries").join(default)]);
                paths
                    .iter()
                    .filter_map(|p| fs::read_to_string(conf.root_path.join(p)).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let grammar_tags = read_all(&conf.tags_filenames, "tags.scm");
            let mut tags = overlay(user_overlays, &name, "tags");
            tags.push('\n');
            tags.push_str(&grammar_tags);
            if tags.trim().is_empty() {
                self.issues.push(format!(
                    "{name}: no tags query; files fall back to text chunks"
                ));
                continue;
            }
            let locals = read_all(&conf.locals_filenames, "locals.scm");
            let tests = overlay(user_overlays, &name, "tests")
                + &fs::read_to_string(conf.root_path.join("queries/tests.scm")).unwrap_or_default();
            match build_language(
                &name,
                grammar,
                &tags,
                &locals,
                &tests,
                conf.file_types.clone(),
                Origin::External(conf.root_path.clone()),
            ) {
                Ok(lang) => self.add(lang),
                Err(e) => self.issues.push(format!("{name}: {e}")),
            }
        }
    }

    pub fn for_path(&self, path: &Path) -> Option<&Arc<Language>> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        self.by_extension.get(&ext).map(|&i| &self.languages[i])
    }

    /// The language named `name`.
    pub fn by_name(&self, name: &str) -> Option<&Arc<Language>> {
        self.languages.iter().find(|l| l.name == name)
    }

    /// The component format of `path`, if it is one.
    pub fn component_for(&self, path: &Path) -> Option<&Arc<Component>> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        self.components
            .iter()
            .find(|c| c.extensions.contains(&ext.as_str()))
    }

    pub fn is_text_extension(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| self.text_extensions.contains(&e.to_lowercase()))
    }

    /// Whether the walker should index this file at all.
    pub fn accepts(&self, path: &Path) -> bool {
        self.for_path(path).is_some()
            || self.component_for(path).is_some()
            || self.is_text_extension(path)
    }

    pub fn languages(&self) -> &[Arc<Language>] {
        &self.languages
    }

    pub fn info(&self) -> Vec<LanguageInfo> {
        let mut exts: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (ext, idx) in &self.by_extension {
            exts.entry(*idx).or_default().push(ext.clone());
        }
        let languages = self.languages.iter().enumerate().map(|(i, l)| {
            let mut e = exts.remove(&i).unwrap_or_default();
            e.sort();
            LanguageInfo {
                name: l.name.clone(),
                origin: match &l.origin {
                    Origin::Builtin => "builtin".into(),
                    Origin::External(p) => p.display().to_string(),
                },
                extensions: e,
                has_tests_query: l.tests.is_some(),
            }
        });
        let components = self.components.iter().map(|c| LanguageInfo {
            name: c.name.to_string(),
            origin: "builtin".into(),
            extensions: c.extensions.iter().map(|e| e.to_string()).collect(),
            has_tests_query: false,
        });
        languages.chain(components).collect()
    }

    /// Identity of everything that shapes chunks: languages, their queries and the extension map.
    pub fn fingerprint(&self) -> String {
        let mut h = blake3::Hasher::new();
        for l in &self.languages {
            h.update(l.query_hash.as_bytes());
        }
        for c in &self.components {
            h.update(format!("{}={}", c.name, c.extensions.join(",")).as_bytes());
        }
        let mut exts: Vec<_> = self
            .by_extension
            .iter()
            .map(|(e, i)| format!("{e}={i}"))
            .collect();
        exts.sort();
        h.update(exts.join(",").as_bytes());
        h.update(self.text_extensions().join(",").as_bytes());
        h.finalize().to_hex().to_string()
    }

    pub fn text_extensions(&self) -> Vec<String> {
        let mut v: Vec<_> = self.text_extensions.iter().cloned().collect();
        v.sort();
        v
    }
}

/// Lodestone's shipped overlay for `lang`/`kind`, preceded by the user's own overlay file.
fn overlay(user_dir: &Path, lang: &str, kind: &str) -> String {
    let mut out =
        fs::read_to_string(user_dir.join(lang).join(format!("{kind}.scm"))).unwrap_or_default();
    if let Some((_, _, text)) = OVERLAYS.iter().find(|(l, k, _)| *l == lang && *k == kind) {
        out.push('\n');
        out.push_str(text);
    }
    out
}

fn build_language(
    name: &str,
    grammar: Grammar,
    tags: &str,
    locals: &str,
    tests: &str,
    extensions: Vec<String>,
    origin: Origin,
) -> anyhow::Result<Language> {
    let tags_config = TagsConfiguration::new(grammar.clone(), tags, locals)
        .map_err(|e| anyhow::anyhow!("tags query: {e}"))?;
    let tests_text = tests.to_string();
    let tests = if tests.trim().is_empty() {
        None
    } else {
        Some(Query::new(&grammar, tests).map_err(|e| anyhow::anyhow!("tests query: {e}"))?)
    };
    let query_hash = blake3::hash(format!("{name}\0{tags}\0{locals}\0{tests_text}").as_bytes())
        .to_hex()
        .to_string();
    Ok(Language {
        name: name.to_string(),
        grammar,
        tags: tags_config,
        tests,
        extensions,
        origin,
        query_hash,
    })
}

pub fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        Err(_) => path.to_path_buf(),
    }
}
