//! Python provider: `pyproject.toml` packages are units; edges are dependencies on sibling
//! packages (names compared normalized, per PEP 503), uv workspace sources and path
//! dependencies (uv, Poetry). Packages come from a uv workspace's members, else from
//! `pyproject.toml` files in the root and up to two levels below it; path dependencies are
//! followed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use super::workspace::{self, Package, strings};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct PythonProvider;

const MANIFEST: &str = "pyproject.toml";

/// PEP 503 normalized name: lowercase, runs of `-`, `_` and `.` as one `-`.
fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// The distribution name a PEP 508 requirement starts with.
fn requirement_name(req: &str) -> Option<String> {
    let req = req.trim_start();
    let end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    (end > 0).then(|| normalize_name(&req[..end]))
}

fn table<'a>(t: &'a Table, path: &[&str]) -> Option<&'a Table> {
    path.iter()
        .try_fold(t, |t, key| t.get(*key).and_then(Value::as_table))
}

/// Records `name`'s source: a sibling by workspace membership or by path.
fn add_source(pkg: &mut Package, dir: &Path, name: &str, source: &Value) {
    let name = normalize_name(name);
    if source.get("workspace").and_then(Value::as_bool) == Some(true) {
        pkg.deps.insert(name);
    } else if let Some(path) = source.get("path").and_then(Value::as_str) {
        pkg.deps.insert(name);
        pkg.dep_paths
            .push(workspace::join(dir, path).join(MANIFEST));
    }
}

fn read(manifest: &Path) -> Option<Package> {
    let t = workspace::read_toml(manifest)?;
    let dir = manifest.parent()?;
    let name = table(&t, &["project"])
        .or_else(|| table(&t, &["tool", "poetry"]))?
        .get("name")?
        .as_str()?;
    let mut pkg = Package::new(name, manifest);
    // PEP 621 / 735 and uv's legacy dev list: requirement strings.
    let mut reqs: Vec<&str> = Vec::new();
    if let Some(project) = table(&t, &["project"]) {
        reqs.extend(strings(project.get("dependencies")));
        for extra in table(project, &["optional-dependencies"])
            .into_iter()
            .flat_map(Table::values)
        {
            reqs.extend(strings(Some(extra)));
        }
    }
    for group in table(&t, &["dependency-groups"])
        .into_iter()
        .flat_map(Table::values)
    {
        reqs.extend(strings(Some(group)));
    }
    reqs.extend(strings(
        table(&t, &["tool", "uv"]).and_then(|uv| uv.get("dev-dependencies")),
    ));
    pkg.deps
        .extend(reqs.into_iter().filter_map(requirement_name));
    for (dep, source) in table(&t, &["tool", "uv", "sources"]).into_iter().flatten() {
        add_source(&mut pkg, dir, dep, source);
    }
    // Poetry: dependency tables keyed by name; a table value may carry a `path`.
    if let Some(poetry) = table(&t, &["tool", "poetry"]) {
        let groups = table(poetry, &["group"])
            .into_iter()
            .flat_map(Table::values)
            .filter_map(|g| g.get("dependencies").and_then(Value::as_table));
        let deps = ["dependencies", "dev-dependencies"]
            .iter()
            .filter_map(|k| table(poetry, &[k]))
            .chain(groups);
        for (dep, spec) in deps.flatten() {
            if dep != "python" {
                pkg.deps.insert(normalize_name(dep));
                add_source(&mut pkg, dir, dep, spec);
            }
        }
    }
    Some(pkg)
}

impl ProjectGraphProvider for PythonProvider {
    fn name(&self) -> &'static str {
        "python"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let root_manifest = root.join(MANIFEST);
        let uv_workspace = workspace::read_toml(&root_manifest)
            .and_then(|t| table(&t, &["tool", "uv", "workspace"]).cloned());
        let seeds: Vec<PathBuf> = match uv_workspace {
            Some(ws) => {
                let excludes = strings(ws.get("exclude")).map(|e| format!("!{e}"));
                let patterns = strings(ws.get("members")).map(String::from).chain(excludes);
                let mut seeds = vec![root_manifest];
                seeds.extend(
                    workspace::members(root, patterns, |d| d.join(MANIFEST).is_file())
                        .into_iter()
                        .map(|d| d.join(MANIFEST)),
                );
                seeds
            }
            None => workspace::nearby(root)
                .into_iter()
                .map(|d| d.join(MANIFEST))
                .filter(|m| m.is_file())
                .collect(),
        };
        let mut packages = workspace::collect(root, seeds, read);
        // Dependencies are recorded normalized; units keep their declared spelling.
        let declared: BTreeMap<String, String> = packages
            .iter()
            .map(|p| (normalize_name(&p.name), p.name.clone()))
            .collect();
        for p in &mut packages {
            p.deps = p
                .deps
                .iter()
                .filter_map(|d| declared.get(d).cloned())
                .collect();
        }
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    #[test]
    fn names_and_requirements() {
        assert_eq!(normalize_name("My__Pkg.name"), "my-pkg-name");
        assert_eq!(
            requirement_name("Foo_Bar[extra]>=1.0; python_version>'3'").as_deref(),
            Some("foo-bar")
        );
        assert_eq!(requirement_name(">=1"), None);
    }

    #[test]
    fn uv_workspace() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"Root_App\"\ndependencies = [\"core-lib\", \"requests>=2\"]\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\nexclude = [\"packages/legacy\"]\n[tool.uv.sources]\ncore-lib = { workspace = true }\n",
                ),
                (
                    "packages/core/pyproject.toml",
                    "[project]\nname = \"core_lib\"\n[dependency-groups]\ndev = [\"Test.Utils\"]\n",
                ),
                (
                    "packages/test-utils/pyproject.toml",
                    "[project]\nname = \"test-utils\"\n",
                ),
                (
                    "packages/legacy/pyproject.toml",
                    "[project]\nname = \"legacy\"\n",
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["python"]);
        assert_eq!(g.units.len(), 3);
        assert_eq!(
            g.unit_of("packages/core/src/core_lib/x.py"),
            Some("core_lib")
        );
        assert_eq!(g.unit_of("scripts/run.py"), Some("Root_App"));
        let allowed = g.allowed_from("Root_App").unwrap();
        assert!(allowed.contains("core_lib") && allowed.contains("test-utils"));
        assert!(!g.deps["Root_App"].contains("requests"));
    }

    #[test]
    fn plain_repo_with_path_dependencies() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                // Tool configuration only: not a package.
                ("pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
                (
                    "services/api/pyproject.toml",
                    "[tool.poetry]\nname = \"api\"\n[tool.poetry.dependencies]\npython = \"^3.12\"\nshared = { path = \"../../libs/py/shared\", develop = true }\n[tool.poetry.group.test.dependencies]\nfixtures = { path = \"../fixtures\" }\n",
                ),
                (
                    "services/fixtures/pyproject.toml",
                    "[project]\nname = \"fixtures\"\n",
                ),
                // Three levels down: found only through the path dependency.
                (
                    "libs/py/shared/pyproject.toml",
                    "[project]\nname = \"shared\"\n[tool.uv.sources]\nbase = { path = \"../base\" }\n",
                ),
                (
                    "libs/py/base/pyproject.toml",
                    "[project]\nname = \"base\"\n",
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        let mut names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["api", "base", "fixtures", "shared"]);
        assert_eq!(
            g.allowed_from("api").unwrap(),
            ["api", "base", "fixtures", "shared"]
                .map(String::from)
                .into()
        );
        assert_eq!(g.unit_of("libs/py/base/base/__init__.py"), Some("base"));
    }
}
