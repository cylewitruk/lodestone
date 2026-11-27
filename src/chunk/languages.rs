//! Language registry: compiled-in grammars plus grammars discovered through tree-sitter's own
//! config. A language is a grammar, its tags query (merged with lodestone's supplementary
//! overlay) and an optional tests query. Component formats (see `component`) are
//! registered alongside. Nothing here knows about any specific language.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::bytes::Regex;
use tree_sitter::{Language as Grammar, Query};
use tree_sitter_tags::TagsConfiguration;

use super::TEXT_LANGUAGE;
use super::component::{self, Component};
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

impl Builtin {
    fn extensions(&self) -> Vec<String> {
        self.extensions.iter().map(|e| e.to_string()).collect()
    }
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
    ("java", "tags", include_str!("../../queries/java/tags.scm")),
    (
        "java",
        "tests",
        include_str!("../../queries/java/tests.scm"),
    ),
    ("c", "tags", include_str!("../../queries/c/tags.scm")),
    ("c", "tests", include_str!("../../queries/c/tests.scm")),
    ("cpp", "tags", include_str!("../../queries/cpp/tags.scm")),
    ("cpp", "tests", include_str!("../../queries/cpp/tests.scm")),
    (
        "csharp",
        "tags",
        include_str!("../../queries/csharp/tags.scm"),
    ),
    (
        "csharp",
        "tests",
        include_str!("../../queries/csharp/tests.scm"),
    ),
    ("ruby", "tags", include_str!("../../queries/ruby/tags.scm")),
    (
        "ruby",
        "tests",
        include_str!("../../queries/ruby/tests.scm"),
    ),
    ("php", "tags", include_str!("../../queries/php/tags.scm")),
    ("php", "tests", include_str!("../../queries/php/tests.scm")),
    ("lua", "tags", include_str!("../../queries/lua/tags.scm")),
    ("lua", "tests", include_str!("../../queries/lua/tests.scm")),
    (
        "elixir",
        "tags",
        include_str!("../../queries/elixir/tags.scm"),
    ),
    (
        "elixir",
        "tests",
        include_str!("../../queries/elixir/tests.scm"),
    ),
    (
        "swift",
        "tags",
        include_str!("../../queries/swift/tags.scm"),
    ),
    (
        "swift",
        "tests",
        include_str!("../../queries/swift/tests.scm"),
    ),
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
    #[cfg(feature = "lang-java")]
    v.push(Builtin {
        name: "java",
        grammar: tree_sitter_java::LANGUAGE.into(),
        tags: vec![tree_sitter_java::TAGS_QUERY],
        locals: "",
        extensions: &["java"],
    });
    #[cfg(feature = "lang-c")]
    v.push(Builtin {
        name: "c",
        grammar: tree_sitter_c::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["c", "h"],
    });
    #[cfg(feature = "lang-cpp")]
    v.push(Builtin {
        name: "cpp",
        grammar: tree_sitter_cpp::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        // `h` only takes effect without C, which registers it first; see `CONTENT_RULES`.
        extensions: &[
            "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ipp", "inl", "h",
        ],
    });
    #[cfg(feature = "lang-csharp")]
    v.push(Builtin {
        name: "csharp",
        grammar: tree_sitter_c_sharp::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["cs"],
    });
    #[cfg(feature = "lang-ruby")]
    v.push(Builtin {
        name: "ruby",
        grammar: tree_sitter_ruby::LANGUAGE.into(),
        tags: vec![tree_sitter_ruby::TAGS_QUERY],
        locals: tree_sitter_ruby::LOCALS_QUERY,
        extensions: &["rb", "rake", "gemspec", "ru"],
    });
    #[cfg(feature = "lang-php")]
    v.push(Builtin {
        name: "php",
        grammar: tree_sitter_php::LANGUAGE_PHP.into(),
        tags: vec![tree_sitter_php::TAGS_QUERY],
        locals: "",
        extensions: &["php"],
    });
    #[cfg(feature = "lang-lua")]
    v.push(Builtin {
        name: "lua",
        grammar: tree_sitter_lua::LANGUAGE.into(),
        tags: vec![tree_sitter_lua::TAGS_QUERY],
        locals: tree_sitter_lua::LOCALS_QUERY,
        extensions: &["lua"],
    });
    #[cfg(feature = "lang-elixir")]
    v.push(Builtin {
        name: "elixir",
        grammar: tree_sitter_elixir::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["ex", "exs"],
    });
    #[cfg(feature = "lang-swift")]
    v.push(Builtin {
        name: "swift",
        grammar: tree_sitter_swift::LANGUAGE.into(),
        tags: vec![],
        locals: "",
        extensions: &["swift"],
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

/// Files of `extension` belong to `language` rather than the extension's own language when their
/// content matches `pattern`.
struct ContentRule {
    extension: &'static str,
    language: &'static str,
    pattern: &'static str,
}

/// `.h` headers are C unless they use C++-only syntax: a class, `enum class`, template,
/// namespace, `using`, access specifier, `std::`, or a standard header without `.h`. A header
/// without any of these parses as C either way.
const CONTENT_RULES: &[ContentRule] = &[ContentRule {
    extension: "h",
    language: "cpp",
    pattern: r"(?m)^[ \t]*(?:class[ \t]+\w|enum[ \t]+class\b|template[ \t]*<|namespace\b|using[ \t]+\w|(?:public|private|protected)[ \t]*:|#[ \t]*include[ \t]*<[\w/]+>)|\bstd::",
}];

/// A [`ContentRule`] whose language is registered.
struct ActiveContentRule {
    extension: String,
    language: usize,
    pattern: Regex,
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
    content_rules: Vec<ActiveContentRule>,
    /// Grammars that were found but could not be used (ABI mismatch, bad query, no tags).
    pub issues: Vec<String>,
}

impl LanguageRegistry {
    /// Builds the registry from built-in grammars, user overlays under `<data_dir>/queries`,
    /// and grammars found via tree-sitter's config. Failures are recorded in `issues`.
    pub fn load(config: &LanguagesConfig, data_dir: &Path) -> Self {
        Self::load_wanted(config, data_dir, &|_, _| true)
    }

    /// [`Self::load`] with only the languages a file named like `path` may be chunked with:
    /// compiling every grammar's queries takes about a second.
    pub fn load_for(config: &LanguagesConfig, data_dir: &Path, path: &Path) -> Self {
        let Some(ext) = extension_of(path) else {
            return Self::load_wanted(config, data_dir, &|_, _| false);
        };
        let mut names: Vec<&str> = config
            .extensions
            .iter()
            .filter(|(e, _)| e.eq_ignore_ascii_case(&ext))
            .map(|(_, lang)| lang.as_str())
            .collect();
        names.extend(
            CONTENT_RULES
                .iter()
                .filter(|r| r.extension == ext)
                .map(|r| r.language),
        );
        if components()
            .iter()
            .any(|c| c.extensions.contains(&ext.as_str()))
        {
            names.extend(component::SCRIPT_LANGUAGES);
        }
        let wanted = |name: &str, extensions: &[String]| {
            names.contains(&name) || extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext))
        };
        Self::load_wanted(config, data_dir, &wanted)
    }

    /// [`Self::load`], building only the languages `wanted` (by name and extensions) accepts.
    fn load_wanted(
        config: &LanguagesConfig,
        data_dir: &Path,
        wanted: &dyn Fn(&str, &[String]) -> bool,
    ) -> Self {
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
            let extensions = b.extensions();
            if !wanted(b.name, &extensions) {
                continue;
            }
            let mut tags_parts = vec![overlay(&user_overlays, b.name, "tags")];
            tags_parts.extend(b.tags.iter().map(|s| s.to_string()));
            let tests = overlay(&user_overlays, b.name, "tests");
            match build_language(
                b.name,
                b.grammar,
                &tags_parts.join("\n"),
                b.locals,
                &tests,
                extensions,
                Origin::Builtin,
            ) {
                Ok(lang) => reg.add(lang),
                Err(e) => reg.issues.push(format!("{}: {e}", b.name)),
            }
        }
        reg.components = components().into_iter().map(Arc::new).collect();
        if let Some(ts_config) = &config.tree_sitter_config {
            reg.load_external(&expand_home(ts_config), data_dir, &user_overlays, wanted);
        }
        for (ext, lang) in &config.extensions {
            if let Some(idx) = reg.languages.iter().position(|l| &l.name == lang) {
                reg.by_extension.insert(ext.to_lowercase(), idx);
            } else {
                reg.issues
                    .push(format!("extension .{ext} maps to unknown language {lang}"));
            }
        }
        // A mapping in config is the user's choice for every file of that extension.
        let mapped: HashSet<String> = config.extensions.keys().map(|e| e.to_lowercase()).collect();
        for rule in CONTENT_RULES {
            let Some(language) = reg.languages.iter().position(|l| l.name == rule.language) else {
                continue;
            };
            if mapped.contains(rule.extension)
                || reg.by_extension.get(rule.extension) == Some(&language)
            {
                continue;
            }
            reg.content_rules.push(ActiveContentRule {
                extension: rule.extension.to_string(),
                language,
                pattern: Regex::new(rule.pattern).expect("built-in content rule pattern"),
            });
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

    fn load_external(
        &mut self,
        ts_config: &Path,
        data_dir: &Path,
        user_overlays: &Path,
        wanted: &dyn Fn(&str, &[String]) -> bool,
    ) {
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
            // A built-in grammar `wanted` left out still wins over an external one.
            if !wanted(&name, &conf.file_types)
                || builtins()
                    .iter()
                    .any(|b| b.name == name && !wanted(b.name, &b.extensions()))
            {
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
        let ext = extension_of(path)?;
        self.by_extension.get(&ext).map(|&i| &self.languages[i])
    }

    /// The language of `path` given its content: a content rule's language when one matches,
    /// else [`Self::for_path`].
    pub fn for_file(&self, path: &Path, source: &[u8]) -> Option<&Arc<Language>> {
        let ext = extension_of(path)?;
        self.content_rules
            .iter()
            .find(|r| r.extension == ext && r.pattern.is_match(source))
            .map(|r| &self.languages[r.language])
            .or_else(|| self.for_path(path))
    }

    /// The language named `name`.
    pub fn by_name(&self, name: &str) -> Option<&Arc<Language>> {
        self.languages.iter().find(|l| l.name == name)
    }

    /// The component format of `path`, if it is one.
    pub fn component_for(&self, path: &Path) -> Option<&Arc<Component>> {
        let ext = extension_of(path)?;
        self.components
            .iter()
            .find(|c| c.extensions.contains(&ext.as_str()))
    }

    pub fn is_text_extension(&self, path: &Path) -> bool {
        extension_of(path).is_some_and(|e| self.text_extensions.contains(&e))
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

    /// Per accepted extension, the identity of what chunks its files: the component (with its
    /// script languages' queries), language (its queries, and any content rule's) or text rule
    /// that [`Chunker::chunk`](super::Chunker::chunk) dispatches to, so a change re-chunks only
    /// the files it affects.
    pub fn extension_fingerprints(&self) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = self
            .text_extensions
            .iter()
            .map(|e| (e.clone(), TEXT_LANGUAGE.to_string()))
            .collect();
        for (ext, &i) in &self.by_extension {
            out.insert(ext.clone(), self.languages[i].query_hash.clone());
        }
        for r in &self.content_rules {
            if let Some(fp) = out.get_mut(&r.extension) {
                let key = format!(
                    "{fp}\0{}\0{}",
                    self.languages[r.language].query_hash,
                    r.pattern.as_str()
                );
                *fp = blake3::hash(key.as_bytes()).to_hex().to_string();
            }
        }
        // Reversed so the first component claiming an extension wins, as in `component_for`.
        for c in self.components.iter().rev() {
            let mut h = blake3::Hasher::new();
            h.update(c.name.as_bytes());
            for guest in component::SCRIPT_LANGUAGES {
                h.update(b"\0");
                h.update(self.by_name(guest).map_or("", |l| &l.query_hash).as_bytes());
            }
            let fp = h.finalize().to_hex().to_string();
            for ext in c.extensions {
                out.insert(ext.to_string(), fp.clone());
            }
        }
        out
    }

    pub fn text_extensions(&self) -> Vec<String> {
        let mut v: Vec<_> = self.text_extensions.iter().cloned().collect();
        v.sort();
        v
    }
}

/// The lowercased extension of `path`, the key of every per-extension lookup.
pub fn extension_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_lowercase())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tempdir;

    fn registry(data_dir: &Path) -> LanguageRegistry {
        let cfg = LanguagesConfig {
            tree_sitter_config: None,
            ..LanguagesConfig::default()
        };
        LanguageRegistry::load(&cfg, data_dir)
    }

    #[cfg(all(
        feature = "lang-rust",
        feature = "lang-c",
        feature = "lang-cpp",
        feature = "lang-svelte"
    ))]
    #[test]
    fn load_for_builds_only_the_languages_a_file_may_need() {
        let cfg = LanguagesConfig {
            tree_sitter_config: None,
            extensions: BTreeMap::from([("RSX".into(), "rust".into())]),
            ..LanguagesConfig::default()
        };
        let names = |file: &str| -> Vec<String> {
            let reg = LanguageRegistry::load_for(&cfg, &tempdir(), Path::new(file));
            reg.languages().iter().map(|l| l.name.clone()).collect()
        };
        assert_eq!(names("src/a.rs"), ["rust"]);
        assert_eq!(names("a.rsx"), ["rust"]);
        // `.h` is C++ when its content says so.
        assert_eq!(names("a.h"), ["c", "cpp"]);
        assert_eq!(names("App.svelte"), ["javascript", "typescript"]);
        assert!(names("notes.unknown").is_empty());
        assert!(names("Makefile").is_empty());
    }

    #[cfg(feature = "lang-svelte")]
    #[test]
    fn extension_fingerprints_follow_what_chunks_each_extension() {
        let before = registry(&tempdir()).extension_fingerprints();
        let data = tempdir();
        fs::create_dir_all(data.join("queries/typescript")).unwrap();
        fs::write(data.join("queries/typescript/tests.scm"), "; overlay\n").unwrap();
        let after = registry(&data).extension_fingerprints();
        let changed: Vec<&str> = before
            .iter()
            .filter(|(ext, fp)| after.get(*ext) != Some(fp))
            .map(|(ext, _)| ext.as_str())
            .collect();
        assert_eq!(changed, ["cts", "mts", "svelte", "ts"]);
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>()
        );
    }
}
