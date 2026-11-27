//! Project unit graph from build-system providers. Providers are detected by marker files and
//! may stack; with none matched the whole repository is one unit and layer filtering is a no-op.
//!
//! In a repo with several ecosystems every matching provider contributes: a file belongs to the
//! unit with the deepest directory containing it, whatever its provider, and edges stay within
//! a provider. Units that share a name (a Python package with a Rust extension, say) or a
//! directory (a crate and an npm package side by side) act as one unit: their files and
//! dependencies combine. Directories cannot tell such units' files apart, and file types would
//! not either for the files they share (docs, configuration, generated bindings).

pub mod cargo;
pub mod dotnet;
pub mod go;
pub mod gradle;
pub mod maven;
pub mod node;
pub mod python;
pub mod workspace;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A buildable unit (package, crate, module) and the repo-relative directories it owns.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub paths: Vec<String>,
    pub provider: String,
}

/// Unit name → names of the units it depends on.
pub type Edges = BTreeMap<String, BTreeSet<String>>;

/// Build-system knowledge behind one trait. `detect` returns `None` when the provider's
/// marker files are absent.
pub trait ProjectGraphProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Units and, for each unit name, the names of units it depends on.
    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectGraph {
    pub units: Vec<Unit>,
    pub deps: Edges,
    pub providers: Vec<String>,
}

pub fn providers() -> Vec<Box<dyn ProjectGraphProvider>> {
    vec![
        Box::new(cargo::CargoProvider),
        Box::new(node::NodeProvider),
        Box::new(go::GoProvider),
        Box::new(python::PythonProvider),
        Box::new(maven::MavenProvider),
        Box::new(gradle::GradleProvider),
        Box::new(dotnet::DotnetProvider),
    ]
}

impl ProjectGraph {
    /// Runs every provider; falls back to a single unit named after the repository root.
    pub fn build(root: &Path) -> Self {
        let mut g = ProjectGraph::default();
        for p in providers() {
            if let Some((units, deps)) = p.detect(root) {
                g.providers.push(p.name().to_string());
                g.units.extend(units);
                for (k, v) in deps {
                    g.deps.entry(k).or_default().extend(v);
                }
            }
        }
        if g.units.is_empty() {
            g.providers.push("fallback".into());
            g.units.push(Unit {
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "root".into()),
                paths: vec![String::new()],
                provider: "fallback".into(),
            });
        }
        g
    }

    /// Whether the layer filter has any effect.
    pub fn has_edges(&self) -> bool {
        !self.providers.iter().all(|p| p == "fallback")
    }

    /// Unit owning `rel_path` (longest matching directory prefix; on a tie, the first unit).
    pub fn unit_of(&self, rel_path: &str) -> Option<&str> {
        self.units
            .iter()
            .flat_map(|u| u.paths.iter().map(move |p| (u, p)))
            .filter(|(_, p)| {
                p.is_empty() || rel_path == p.as_str() || rel_path.starts_with(&format!("{p}/"))
            })
            .reduce(|best, x| if x.1.len() > best.1.len() { x } else { best })
            .map(|(u, _)| u.name.as_str())
    }

    /// `unit` plus every unit it transitively depends on, or `None` if `unit` is unknown. Units
    /// sharing a directory are reached together, since [`Self::unit_of`] gives its files to one
    /// of them.
    pub fn allowed_from(&self, unit: &str) -> Option<BTreeSet<String>> {
        if !self.units.iter().any(|u| u.name == unit) {
            return None;
        }
        let mut at_path: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for u in &self.units {
            for p in &u.paths {
                at_path.entry(p).or_default().insert(&u.name);
            }
        }
        let co_located = |name: &str| -> BTreeSet<&str> {
            self.units
                .iter()
                .filter(|u| u.name == name)
                .flat_map(|u| &u.paths)
                .flat_map(|p| at_path[p.as_str()].iter().copied())
                .collect()
        };
        let mut seen = BTreeSet::from([unit.to_string()]);
        let mut queue = VecDeque::from([unit.to_string()]);
        while let Some(u) = queue.pop_front() {
            let deps = self.deps.get(&u).into_iter().flatten().map(String::as_str);
            for d in deps.chain(co_located(&u)) {
                if seen.insert(d.to_string()) {
                    queue.push_back(d.to_string());
                }
            }
        }
        Some(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_and_unit_lookup() {
        let g = ProjectGraph {
            units: vec![
                Unit {
                    name: "a".into(),
                    paths: vec!["a".into()],
                    provider: "t".into(),
                },
                Unit {
                    name: "b".into(),
                    paths: vec!["b".into()],
                    provider: "t".into(),
                },
                Unit {
                    name: "c".into(),
                    paths: vec!["c".into()],
                    provider: "t".into(),
                },
                Unit {
                    name: "b-sub".into(),
                    paths: vec!["b/sub".into()],
                    provider: "t".into(),
                },
            ],
            deps: BTreeMap::from([
                ("a".into(), BTreeSet::from(["b".into()])),
                ("b".into(), BTreeSet::from(["c".into()])),
            ]),
            providers: vec!["t".into()],
        };
        assert_eq!(
            g.allowed_from("a").unwrap(),
            BTreeSet::from(["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(g.allowed_from("c").unwrap(), BTreeSet::from(["c".into()]));
        assert!(g.allowed_from("zzz").is_none());
        assert_eq!(g.unit_of("b/sub/x.rs"), Some("b-sub"));
        assert_eq!(g.unit_of("b/x.rs"), Some("b"));
        assert_eq!(g.unit_of("bb/x.rs"), None);
    }

    #[test]
    fn mixed_repo_combines_providers() {
        let root = crate::testutil::tempdir();
        crate::testutil::write_tree(
            &root,
            &[
                // A Python package with a Rust extension, both named `fastlib`.
                ("pyproject.toml", "[project]\nname = \"fastlib\"\n"),
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"native\", \"helper\"]\n",
                ),
                ("helper/Cargo.toml", "[package]\nname = \"helper\"\n"),
                (
                    "native/Cargo.toml",
                    "[package]\nname = \"fastlib\"\n[dependencies]\nhelper = { path = \"../helper\" }\n",
                ),
                ("tools/gen/go.mod", "module example.com/gen\n"),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["cargo", "go", "python"]);
        assert_eq!(g.unit_of("native/src/lib.rs"), Some("fastlib"));
        assert_eq!(g.unit_of("fastlib/__init__.py"), Some("fastlib"));
        assert_eq!(g.unit_of("tools/gen/main.go"), Some("example.com/gen"));
        assert_eq!(
            g.allowed_from("fastlib").unwrap(),
            BTreeSet::from(["fastlib".into(), "helper".into()])
        );
    }

    #[test]
    fn co_located_units_act_as_one() {
        let root = crate::testutil::tempdir();
        crate::testutil::write_tree(
            &root,
            &[
                ("Cargo.toml", "[package]\nname = \"native\"\n"),
                (
                    "package.json",
                    r#"{"name": "frontend", "workspaces": ["ui"]}"#,
                ),
                ("ui/package.json", r#"{"name": "ui"}"#),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(
            g.unit_of("src/lib.rs"),
            Some("native"),
            "the first provider's"
        );
        let both = BTreeSet::from(["native".to_string(), "frontend".to_string()]);
        assert_eq!(g.allowed_from("native").unwrap(), both);
        assert_eq!(g.allowed_from("frontend").unwrap(), both);
        assert_eq!(
            g.allowed_from("ui").unwrap(),
            BTreeSet::from(["ui".to_string()])
        );
    }

    #[test]
    fn fallback_is_single_unit() {
        let dir = crate::testutil::tempdir();
        let g = ProjectGraph::build(&dir);
        assert_eq!(g.units.len(), 1);
        assert!(!g.has_edges());
        assert!(g.unit_of("anything/here.py").is_some());
    }
}
