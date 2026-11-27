//! Cargo provider: packages are units; edges are dependencies on sibling packages.

use std::collections::BTreeSet;
use std::path::Path;

use toml::Table;

use super::workspace::{self, Package};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct CargoProvider;

const DEP_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

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
        let root_manifest = workspace::read_toml(&root.join("Cargo.toml"))?;
        let mut dirs = Vec::new();
        if let Some(ws) = root_manifest.get("workspace").and_then(|w| w.as_table()) {
            let excludes = workspace::strings(ws.get("exclude")).map(|e| format!("!{e}"));
            let patterns = workspace::strings(ws.get("members"))
                .map(String::from)
                .chain(excludes);
            dirs = workspace::members(root, patterns, |d| d.join("Cargo.toml").is_file());
        }
        if root_manifest.contains_key("package") && !dirs.iter().any(|d| d == root) {
            dirs.push(root.to_path_buf());
        }
        let packages = dirs
            .into_iter()
            .filter_map(|dir| {
                let manifest = dir.join("Cargo.toml");
                let m = workspace::read_toml(&manifest)?;
                let name = m.get("package")?.get("name")?.as_str()?;
                Some(Package {
                    deps: dep_names(&m),
                    ..Package::new(name, &manifest)
                })
            })
            .collect();
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    #[test]
    fn workspace_with_globs_and_renames() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"app\", \"libs/*\"]\nexclude = [\"libs/skip\"]\n[workspace.dependencies]\ncore = { path = \"libs/core\" }\n",
                ),
                (
                    "app/Cargo.toml",
                    "[package]\nname = \"app\"\n[dependencies]\nutil = { path = \"../libs/util\" }\nserde = \"1\"\n",
                ),
                (
                    "libs/util/Cargo.toml",
                    "[package]\nname = \"util\"\n[dependencies]\nmycore = { workspace = true, package = \"core\" }\n",
                ),
                (
                    "libs/core/Cargo.toml",
                    "[package]\nname = \"core\"\n[target.'cfg(unix)'.dev-dependencies]\nutil = { path = \"../util\" }\n",
                ),
                ("libs/skip/Cargo.toml", "[package]\nname = \"skip\"\n"),
            ],
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
