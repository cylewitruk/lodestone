//! Go provider: modules are units, named by module path; edges are `require` and local
//! `replace` directives naming sibling modules. Modules come from `go.work`, else from `go.mod`
//! files in the root and up to two levels below it; local `replace` targets (a module's own, and
//! `go.work`'s for the modules it requires) are followed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::workspace::{self, Package};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct GoProvider;

/// `(verb, arguments)` per directive of a `go.mod` or `go.work`, `verb ( ... )` blocks expanded.
fn directives(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut block: Option<&str> = None;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(verb) = block {
            if line == ")" {
                block = None;
            } else {
                out.push((verb, line));
            }
            continue;
        }
        let end = line
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(line.len());
        let (verb, rest) = (&line[..end], line[end..].trim());
        if rest == "(" {
            block = Some(verb);
        } else {
            out.push((verb, rest));
        }
    }
    out
}

fn first_arg(args: &str) -> Option<&str> {
    args.split_whitespace().next().map(|a| a.trim_matches('"'))
}

fn is_local(path: &str) -> bool {
    path.starts_with("./") || path.starts_with("../") || Path::new(path).is_absolute()
}

/// Local `replace` targets among `directives`: module path → its `go.mod`.
fn local_replaces(directives: &[(&str, &str)], dir: &Path) -> BTreeMap<String, PathBuf> {
    directives
        .iter()
        .filter(|(verb, _)| *verb == "replace")
        .filter_map(|(_, args)| {
            let (old, new) = args.split_once("=>")?;
            let (old, new) = (first_arg(old)?, first_arg(new)?);
            is_local(new).then(|| (old.to_string(), workspace::join(dir, new).join("go.mod")))
        })
        .collect()
}

/// A module; `work_replaces` are `go.work`'s, which apply to every module it uses.
fn read(manifest: &Path, work_replaces: &BTreeMap<String, PathBuf>) -> Option<Package> {
    let text = fs::read_to_string(manifest).ok()?;
    let dir = manifest.parent()?;
    let directives = directives(&text);
    let name = directives
        .iter()
        .find(|(verb, _)| *verb == "module")
        .and_then(|(_, args)| first_arg(args))?;
    let mut pkg = Package::new(name, manifest);
    pkg.deps.extend(
        directives
            .iter()
            .filter(|(verb, _)| *verb == "require")
            .filter_map(|(_, args)| first_arg(args).map(String::from)),
    );
    let required: Vec<(String, PathBuf)> = work_replaces
        .iter()
        .filter(|(module, _)| pkg.deps.contains(*module))
        .map(|(m, p)| (m.clone(), p.clone()))
        .collect();
    for (module, go_mod) in local_replaces(&directives, dir).into_iter().chain(required) {
        pkg.deps.insert(module);
        pkg.dep_paths.push(go_mod);
    }
    Some(pkg)
}

/// Directories Go itself skips when walking a module.
fn skipped(root: &Path, dir: &Path) -> bool {
    workspace::rel(root, dir)
        .split('/')
        .any(|s| s == "testdata" || s == "vendor" || s.starts_with('_'))
}

impl ProjectGraphProvider for GoProvider {
    fn name(&self) -> &'static str {
        "go"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let (seeds, work_replaces): (Vec<PathBuf>, _) =
            match fs::read_to_string(root.join("go.work")) {
                Ok(work) => {
                    let work = directives(&work);
                    let seeds = work
                        .iter()
                        .filter(|(verb, _)| *verb == "use")
                        .filter_map(|(_, args)| first_arg(args))
                        .map(|d| workspace::join(root, d).join("go.mod"))
                        .collect();
                    (seeds, local_replaces(&work, root))
                }
                Err(_) => (
                    workspace::nearby(root)
                        .into_iter()
                        .filter(|d| !skipped(root, d))
                        .map(|d| d.join("go.mod"))
                        .filter(|m| m.is_file())
                        .collect(),
                    BTreeMap::new(),
                ),
            };
        let packages = workspace::collect(root, seeds, |m| read(m, &work_replaces));
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    #[test]
    fn parses_blocks_and_single_lines() {
        let d = directives(
            "module example.com/app // the app\n\ngo 1.22\n\nrequire (\n\texample.com/lib v0.0.0\n\tgithub.com/x/y v1.2.3 // indirect\n)\nreplace example.com/lib => ../lib\n",
        );
        assert_eq!(
            d,
            [
                ("module", "example.com/app"),
                ("go", "1.22"),
                ("require", "example.com/lib v0.0.0"),
                ("require", "github.com/x/y v1.2.3"),
                ("replace", "example.com/lib => ../lib"),
            ]
        );
    }

    #[test]
    fn go_work_modules() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "go.work",
                    "go 1.22\n\nuse (\n\t./app\n\t./lib\n)\nuse ./util\n",
                ),
                (
                    "app/go.mod",
                    "module example.com/app\n\nrequire example.com/lib v0.0.0\n",
                ),
                (
                    "lib/go.mod",
                    "module example.com/lib\n\nrequire (\n\texample.com/util v0.0.0\n\tgolang.org/x/text v0.3.0\n)\n",
                ),
                ("util/go.mod", "module example.com/util\n"),
                ("other/go.mod", "module example.com/other\n"),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["go"]);
        assert_eq!(g.units.len(), 3, "only go.work's modules");
        assert_eq!(g.unit_of("lib/x/y.go"), Some("example.com/lib"));
        let allowed = g.allowed_from("example.com/app").unwrap();
        assert!(allowed.contains("example.com/util"));
        assert!(!g.deps["example.com/lib"].contains("golang.org/x/text"));
    }

    #[test]
    fn go_work_replaces_apply_to_used_modules() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "go.work",
                    "go 1.22\n\nuse ./app\n\nreplace (\n\texample.com/core => ./lib/core\n\texample.com/unused => ./lib/unused\n)\n",
                ),
                (
                    "app/go.mod",
                    "module example.com/app\n\nrequire example.com/core v0.0.0\n",
                ),
                ("lib/core/go.mod", "module example.com/core\n"),
                ("lib/unused/go.mod", "module example.com/unused\n"),
            ],
        );
        let g = ProjectGraph::build(&root);
        let names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["example.com/app", "example.com/core"]);
        assert!(g.deps["example.com/app"].contains("example.com/core"));
        assert_eq!(g.unit_of("lib/core/core.go"), Some("example.com/core"));
    }

    #[test]
    fn nested_modules_and_local_replaces() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "go.mod",
                    "module example.com/root\n\nreplace example.com/deep => ./internal/pkg/deep\n",
                ),
                (
                    "services/api/go.mod",
                    "module example.com/api\n\nrequire example.com/root v0.0.0\n",
                ),
                ("internal/pkg/deep/go.mod", "module example.com/deep\n"),
                ("testdata/mod/go.mod", "module example.com/fixture\n"),
            ],
        );
        let g = ProjectGraph::build(&root);
        let names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            ["example.com/root", "example.com/api", "example.com/deep"]
        );
        assert_eq!(
            g.unit_of("internal/pkg/deep/d.go"),
            Some("example.com/deep")
        );
        assert_eq!(g.unit_of("internal/x.go"), Some("example.com/root"));
        assert!(
            g.allowed_from("example.com/api")
                .unwrap()
                .contains("example.com/deep")
        );
    }
}
