//! Cargo provider: packages are units; edges are dependencies on sibling packages.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use toml::Table;

use super::{Edges, ProjectGraphProvider, Unit};

pub struct CargoProvider;

const DEP_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

fn read_manifest(path: &Path) -> Option<Table> {
    fs::read_to_string(path).ok()?.parse::<Table>().ok()
}

/// Expands workspace member patterns; supports `*` as a whole path segment or a segment suffix.
fn expand_members(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    for seg in pattern.split('/') {
        let mut next = Vec::new();
        for p in &paths {
            if seg.contains('*') {
                let (pre, post) = seg.split_once('*').unwrap_or((seg, ""));
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() && name.starts_with(pre) && name.ends_with(post) {
                            next.push(e.path());
                        }
                    }
                }
            } else {
                next.push(p.join(seg));
            }
        }
        paths = next;
    }
    paths.retain(|p| p.join("Cargo.toml").is_file());
    paths.sort();
    paths
}

/// Names of packages a manifest depends on (respecting `package = "..."` renames).
fn dep_names(manifest: &Table) -> BTreeSet<String> {
    let mut tables: Vec<&Table> = DEP_TABLES
        .iter()
        .filter_map(|t| manifest.get(*t).and_then(|v| v.as_table()))
        .collect();
    if let Some(targets) = manifest.get("target").and_then(|v| v.as_table()) {
        for t in targets.values().filter_map(|v| v.as_table()) {
            tables.extend(
                DEP_TABLES
                    .iter()
                    .filter_map(|d| t.get(*d).and_then(|v| v.as_table())),
            );
        }
    }
    let mut out = BTreeSet::new();
    for table in tables {
        for (key, value) in table {
            let name = value
                .as_table()
                .and_then(|t| t.get("package"))
                .and_then(|p| p.as_str())
                .unwrap_or(key);
            out.insert(name.to_string());
        }
    }
    out
}

impl ProjectGraphProvider for CargoProvider {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let root_manifest = read_manifest(&root.join("Cargo.toml"))?;
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(ws) = root_manifest.get("workspace").and_then(|w| w.as_table()) {
            let excluded: Vec<PathBuf> = ws
                .get("exclude")
                .and_then(|e| e.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .map(|s| root.join(s))
                .collect();
            for m in ws
                .get("members")
                .and_then(|m| m.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
            {
                for d in expand_members(root, m) {
                    if !excluded.contains(&d) && !dirs.contains(&d) {
                        dirs.push(d);
                    }
                }
            }
        }
        if root_manifest.contains_key("package") && !dirs.iter().any(|d| d == root) {
            dirs.push(root.to_path_buf());
        }
        let mut manifests = Vec::new();
        for dir in dirs {
            let Some(m) = read_manifest(&dir.join("Cargo.toml")) else {
                continue;
            };
            let Some(name) = m
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
            else {
                continue;
            };
            let rel = dir
                .strip_prefix(root)
                .unwrap_or(&dir)
                .to_string_lossy()
                .replace('\\', "/");
            manifests.push((name, rel, m));
        }
        if manifests.is_empty() {
            return None;
        }
        let names: BTreeSet<String> = manifests.iter().map(|(n, _, _)| n.clone()).collect();
        let mut deps = BTreeMap::new();
        let mut units = Vec::new();
        for (name, rel, m) in manifests {
            let edges: BTreeSet<String> = dep_names(&m)
                .into_iter()
                .filter(|d| names.contains(d) && d != &name)
                .collect();
            deps.insert(name.clone(), edges);
            units.push(Unit {
                name,
                paths: vec![rel],
                provider: "cargo".into(),
            });
        }
        Some((units, deps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;

    #[test]
    fn workspace_with_globs_and_renames() {
        let root = crate::testutil::tempdir();
        let w = |p: &str, s: &str| {
            let path = root.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, s).unwrap();
        };
        w(
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"libs/*\"]\n[workspace.dependencies]\ncore = { path = \"libs/core\" }\n",
        );
        w(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n[dependencies]\nutil = { path = \"../libs/util\" }\nserde = \"1\"\n",
        );
        w(
            "libs/util/Cargo.toml",
            "[package]\nname = \"util\"\n[dependencies]\nmycore = { workspace = true, package = \"core\" }\n",
        );
        w(
            "libs/core/Cargo.toml",
            "[package]\nname = \"core\"\n[target.'cfg(unix)'.dev-dependencies]\nutil = { path = \"../util\" }\n",
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, vec!["cargo"]);
        assert_eq!(g.units.len(), 3);
        assert_eq!(g.unit_of("libs/util/src/lib.rs"), Some("util"));
        let allowed = g.allowed_from("app").unwrap();
        assert!(allowed.contains("util") && allowed.contains("core"));
        assert!(g.deps["core"].contains("util"));
        assert!(!g.deps["app"].contains("serde"));
    }
}
