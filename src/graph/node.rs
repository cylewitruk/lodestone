//! JS/TS provider: npm, yarn and pnpm workspace packages are units; edges are dependencies on
//! sibling packages, by name or through the `workspace:` protocol.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::workspace::{self, Package};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct NodeProvider;

const DEP_FIELDS: &[&str] = &["dependencies", "devDependencies", "peerDependencies"];

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// `s` split at each `sep` outside single or double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == sep => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            None => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `packages:` entries of a `pnpm-workspace.yaml` (block or flow list).
fn pnpm_packages(text: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches(['"', '\'']).to_string();
    let mut out = Vec::new();
    let mut in_packages = false;
    for raw in text.lines() {
        let line = raw.split(" #").next().unwrap_or("").trim_end();
        let item = line.trim_start();
        if item.is_empty() || item.starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t']) && !item.starts_with('-') {
            in_packages = false;
            if let Some(rest) = item.strip_prefix("packages:").map(str::trim) {
                match rest.strip_prefix('[') {
                    Some(flow) => out.extend(
                        split_unquoted(flow.trim_end_matches(']'), ',')
                            .into_iter()
                            .map(unquote)
                            .filter(|s| !s.is_empty()),
                    ),
                    None => in_packages = true,
                }
            }
        } else if in_packages && let Some(entry) = item.strip_prefix('-') {
            out.push(unquote(entry));
        }
    }
    out
}

/// Names a `package.json` depends on; a `workspace:name@range` alias names `name`.
fn dep_names(pkg: &Value) -> BTreeSet<String> {
    DEP_FIELDS
        .iter()
        .filter_map(|f| pkg.get(*f).and_then(Value::as_object))
        .flatten()
        .map(|(key, spec)| {
            spec.as_str()
                .and_then(|s| s.strip_prefix("workspace:"))
                .and_then(|s| s.rsplit_once('@'))
                .map(|(alias, _)| alias)
                .filter(|a| !a.is_empty())
                .unwrap_or(key)
                .to_string()
        })
        .collect()
}

impl ProjectGraphProvider for NodeProvider {
    fn name(&self) -> &'static str {
        "node"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let root_pkg = read_json(&root.join("package.json"));
        let mut patterns: Vec<String> = match root_pkg.as_ref().and_then(|p| p.get("workspaces")) {
            // Yarn classic also takes `{ "packages": [...] }`.
            Some(Value::Object(o)) => o.get("packages").cloned().unwrap_or_default(),
            Some(v) => v.clone(),
            None => Value::Null,
        }
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
        if let Ok(text) = fs::read_to_string(root.join("pnpm-workspace.yaml")) {
            patterns.extend(pnpm_packages(&text));
        }
        let mut dirs = workspace::members(root, patterns, |d| d.join("package.json").is_file());
        if root_pkg.is_some() && !dirs.iter().any(|d| d == root) {
            dirs.push(root.to_path_buf());
        }
        let packages = dirs
            .into_iter()
            .filter_map(|dir| {
                let manifest = dir.join("package.json");
                let pkg = read_json(&manifest)?;
                let name = pkg.get("name")?.as_str()?;
                Some(Package {
                    deps: dep_names(&pkg),
                    ..Package::new(name, &manifest)
                })
            })
            .collect();
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    #[test]
    fn npm_workspaces_and_aliases() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "package.json",
                    r#"{"name": "mono", "private": true, "workspaces": ["packages/*", "apps/web"]}"#,
                ),
                (
                    "apps/web/package.json",
                    r#"{"name": "@acme/web", "dependencies": {"@acme/ui": "workspace:*", "react": "^19"}}"#,
                ),
                (
                    "packages/ui/package.json",
                    r#"{"name": "@acme/ui", "peerDependencies": {"core-alias": "workspace:@acme/core@^1"}}"#,
                ),
                (
                    "packages/core/package.json",
                    r#"{"name": "@acme/core", "devDependencies": {"@acme/ui": "1.0.0"}}"#,
                ),
                (
                    "packages/core/node_modules/x/package.json",
                    r#"{"name": "x"}"#,
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["node"]);
        assert_eq!(g.units.len(), 4);
        assert_eq!(g.unit_of("apps/web/src/main.ts"), Some("@acme/web"));
        assert_eq!(g.unit_of("scripts/build.js"), Some("mono"));
        assert_eq!(
            g.allowed_from("@acme/web").unwrap(),
            BTreeSet::from(["@acme/web", "@acme/ui", "@acme/core"].map(String::from))
        );
        assert!(g.deps["@acme/core"].contains("@acme/ui"));
    }

    #[test]
    fn pnpm_and_yarn_object_form() {
        assert_eq!(
            pnpm_packages(
                "# comment\npackages:\n  - 'packages/*'\n  - \"!**/test/**\" # skip\n  - tools\ncatalog:\n  - nope\n"
            ),
            ["packages/*", "!**/test/**", "tools"]
        );
        assert_eq!(pnpm_packages("packages: ['a/*', \"b\"]\n"), ["a/*", "b"]);
        assert_eq!(
            pnpm_packages("packages: ['{apps,packages}/*', \"x,y\", z]\n"),
            ["{apps,packages}/*", "x,y", "z"]
        );
        let braces = tempdir();
        write_tree(
            &braces,
            &[
                ("pnpm-workspace.yaml", "packages: ['{apps,packages}/*']\n"),
                ("apps/web/package.json", r#"{"name": "web"}"#),
                ("packages/ui/package.json", r#"{"name": "ui"}"#),
            ],
        );
        assert_eq!(ProjectGraph::build(&braces).units.len(), 2);

        let root = tempdir();
        write_tree(
            &root,
            &[
                ("pnpm-workspace.yaml", "packages:\n  - libs/*\n"),
                (
                    "libs/a/package.json",
                    r#"{"name": "a", "dependencies": {"b": "workspace:^"}}"#,
                ),
                ("libs/b/package.json", r#"{"name": "b"}"#),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.units.len(), 2, "no root package.json, no root unit");
        assert!(g.deps["a"].contains("b"));

        let root = tempdir();
        write_tree(
            &root,
            &[
                ("package.json", r#"{"workspaces": {"packages": ["p/*"]}}"#),
                ("p/one/package.json", r#"{"name": "one"}"#),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.units.len(), 1, "an unnamed root package is no unit");
    }
}
