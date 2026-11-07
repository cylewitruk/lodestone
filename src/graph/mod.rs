//! Project unit graph from build-system providers. Providers are detected by marker files and
//! may stack; with none matched the whole repository is one unit and layer filtering is a no-op.

pub mod cargo;

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
    vec![Box::new(cargo::CargoProvider)]
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

    /// Unit owning `rel_path` (longest matching directory prefix).
    pub fn unit_of(&self, rel_path: &str) -> Option<&str> {
        self.units
            .iter()
            .flat_map(|u| u.paths.iter().map(move |p| (u, p)))
            .filter(|(_, p)| {
                p.is_empty() || rel_path == p.as_str() || rel_path.starts_with(&format!("{p}/"))
            })
            .max_by_key(|(_, p)| p.len())
            .map(|(u, _)| u.name.as_str())
    }

    /// `unit` plus every unit it transitively depends on, or `None` if `unit` is unknown.
    pub fn allowed_from(&self, unit: &str) -> Option<BTreeSet<String>> {
        if !self.units.iter().any(|u| u.name == unit) {
            return None;
        }
        let mut seen = BTreeSet::from([unit.to_string()]);
        let mut queue = VecDeque::from([unit.to_string()]);
        while let Some(u) = queue.pop_front() {
            for d in self.deps.get(&u).into_iter().flatten() {
                if seen.insert(d.clone()) {
                    queue.push_back(d.clone());
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
    fn fallback_is_single_unit() {
        let dir = crate::testutil::tempdir();
        let g = ProjectGraph::build(&dir);
        assert_eq!(g.units.len(), 1);
        assert!(!g.has_edges());
        assert!(g.unit_of("anything/here.py").is_some());
    }
}
