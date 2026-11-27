//! Gradle provider: projects `include`d by `settings.gradle(.kts)` are units named by project
//! path (`:libs:core`; the root project, when it has a build file, is `:`); edges are
//! `project(":x")` references and type-safe `projects.x` accessors in a project's build file, or
//! in a `project(":x") {}`, `subprojects {}` or `allprojects {}` block configuring it from
//! another. Both DSLs are scanned with regexes and brace depth, not evaluated: only literal
//! paths are understood.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::workspace::{self, Package};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct GradleProvider;

static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/|//[^\n]*").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"["']([^"']*)["']"#).unwrap());
/// `include ':a', ':b'` and `include(":a", ":b")`, possibly over several lines.
static INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\binclude\b\s*\(?\s*((?:["'][^"']*["']\s*,?\s*)+)"#).unwrap());
/// `project(":x").projectDir = file("dir")` or `new File(settingsDir, "dir")`.
static PROJECT_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"project\s*\(\s*["'](:?[^"']*)["']\s*\)\s*\.\s*projectDir\s*=\s*(?:file\s*\(|new\s+File\s*\(\s*(?:settingsDir|rootDir)\s*,)\s*["']([^"']+)["']"#,
    )
    .unwrap()
});
/// `project(":x")`, `project(path = ":x")`, `project(path: ':x', ...)`.
static PROJECT_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bproject\s*\(\s*(?:path\s*[:=]\s*)?["'](:[^"']*)["']"#).unwrap()
});
/// `project(":x") {`, `subprojects {` and `allprojects {`.
static SCOPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\b(?:project\s*\(\s*["'](:?[^"']*)["']\s*\)|(subprojects|allprojects))\s*\{"#,
    )
    .unwrap()
});
static ACCESSOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bprojects((?:\.[A-Za-z_][A-Za-z0-9_]*)+)").unwrap());

const SETTINGS: &[&str] = &["settings.gradle.kts", "settings.gradle"];
const BUILD: &[&str] = &["build.gradle.kts", "build.gradle"];

fn read_script(dir: &Path, names: &[&str]) -> Option<(PathBuf, String)> {
    names.iter().map(|n| dir.join(n)).find_map(|p| {
        let text = fs::read_to_string(&p).ok()?;
        Some((p, COMMENT.replace_all(&text, "").into_owned()))
    })
}

/// A project path with its leading `:`.
fn absolute(path: &str) -> String {
    format!(":{}", path.trim_start_matches(':'))
}

/// The type-safe accessor of a project path: `:libs:my-lib` → `.libs.myLib`.
fn accessor(path: &str) -> String {
    let mut out = String::new();
    for seg in path.split(':').filter(|s| !s.is_empty()) {
        out.push('.');
        for (i, word) in seg.split(['-', '_']).enumerate() {
            let mut chars = word.chars();
            match (i, chars.next()) {
                (0, Some(c)) => out.push(c),
                (_, Some(c)) => out.push(c.to_ascii_uppercase()),
                _ => {}
            }
            out.extend(chars);
        }
    }
    out
}

/// A block configuring other projects.
struct Scope {
    /// `project(":x")`, `subprojects` or `allprojects`, through the `{`.
    selector: Range<usize>,
    body: Range<usize>,
    targets: Vec<String>,
}

/// Index of the `}` closing the block whose body starts at `start` (the text's end if none).
fn block_end(text: &str, start: usize) -> usize {
    let mut depth = 1;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return start + i;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// `(project, dependency)` pairs referenced by `owner`'s build script `text`. References inside
/// `project(":x") {}`, `subprojects {}` and `allprojects {}` blocks belong to the projects those
/// configure; the innermost block wins.
fn script_deps(
    text: &str,
    owner: &str,
    projects: &[String],
    accessors: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let below = |p: &str| -> Vec<String> {
        let prefix = if p == ":" { ":".into() } else { format!("{p}:") };
        projects
            .iter()
            .filter(|q| q.starts_with(&prefix) && q.as_str() != p)
            .cloned()
            .collect()
    };
    let scopes: Vec<Scope> = SCOPE
        .captures_iter(text)
        .filter_map(|c| {
            let m = c.get(0)?;
            let targets = match (c.get(1), c.get(2).map(|k| k.as_str())) {
                (Some(p), _) => vec![absolute(p.as_str())],
                (None, Some("subprojects")) => below(owner),
                _ => [owner.to_string()].into_iter().chain(below(owner)).collect(),
            };
            Some(Scope {
                selector: m.range(),
                body: m.end()..block_end(text, m.end()),
                targets,
            })
        })
        .collect();
    let refs = PROJECT_REF
        .captures_iter(text)
        .filter_map(|c| Some((c.get(0)?.start(), c[1].to_string())))
        .chain(
            ACCESSOR
                .captures_iter(text)
                .filter_map(|c| Some((c.get(0)?.start(), accessors.get(&c[1])?.clone()))),
        );
    let mut out = Vec::new();
    for (pos, dep) in refs {
        if scopes.iter().any(|s| s.selector.contains(&pos)) {
            continue;
        }
        let targets = scopes
            .iter()
            .filter(|s| s.body.contains(&pos))
            .max_by_key(|s| s.body.start)
            .map_or_else(|| vec![owner.to_string()], |s| s.targets.clone());
        out.extend(targets.into_iter().map(|t| (t, dep.clone())));
    }
    out
}

impl ProjectGraphProvider for GradleProvider {
    fn name(&self) -> &'static str {
        "gradle"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let settings = read_script(root, SETTINGS).map(|(_, text)| text);
        if settings.is_none() && read_script(root, BUILD).is_none() {
            return None;
        }
        let settings = settings.unwrap_or_default();
        let dirs: BTreeMap<String, String> = PROJECT_DIR
            .captures_iter(&settings)
            .map(|c| (absolute(&c[1]), c[2].to_string()))
            .collect();
        let mut projects: Vec<(String, PathBuf)> = vec![(":".into(), root.to_path_buf())];
        for args in INCLUDE.captures_iter(&settings) {
            for q in QUOTED.captures_iter(&args[1]) {
                let path = absolute(&q[1]);
                // Including `:a:b` also creates `:a`.
                let ancestors = path.match_indices(':').skip(1).map(|(i, _)| &path[..i]);
                for p in ancestors.chain([path.as_str()]) {
                    if projects.iter().any(|(q, _)| q == p) {
                        continue;
                    }
                    let dir = match dirs.get(p) {
                        Some(d) => workspace::join(root, d),
                        None => root.join(p.trim_start_matches(':').replace(':', "/")),
                    };
                    projects.push((p.to_string(), dir));
                }
            }
        }
        let names: Vec<String> = projects.iter().map(|(p, _)| p.clone()).collect();
        let accessors: BTreeMap<String, String> =
            names.iter().map(|p| (accessor(p), p.clone())).collect();
        // A build script may configure other projects, so dependencies are gathered first.
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut packages = Vec::new();
        for (path, dir) in projects {
            let script = read_script(&dir, BUILD);
            // The root is a unit only with a build file of its own.
            if path == ":" && script.is_none() {
                continue;
            }
            let (manifest, text) = script.unwrap_or_else(|| (dir.join(BUILD[1]), String::new()));
            for (target, dep) in script_deps(&text, &path, &names, &accessors) {
                deps.entry(target).or_default().insert(dep);
            }
            packages.push(Package::new(path, &manifest));
        }
        for p in &mut packages {
            p.deps = deps.remove(&p.name).unwrap_or_default();
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
    fn accessors() {
        assert_eq!(accessor(":libs:my-lib"), ".libs.myLib");
        assert_eq!(accessor(":snake_case"), ".snakeCase");
    }

    #[test]
    fn kotlin_dsl() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "settings.gradle.kts",
                    "rootProject.name = \"acme\"\ninclude(\n    \":app\",\n    \":libs:core\", // the core\n)\ninclude(\":libs:my-util\")\n// include(\":dead\")\ninclude(\":tool\")\nproject(\":tool\").projectDir = file(\"tools/cli\")\nincludeBuild(\"build-logic\")\n",
                ),
                (
                    "app/build.gradle.kts",
                    "dependencies {\n    implementation(project(\":libs:core\"))\n    testImplementation(projects.libs.myUtil)\n}\n",
                ),
                (
                    "libs/core/build.gradle.kts",
                    "dependencies { api(project(path = \":tool\")) }\n",
                ),
                ("tools/cli/build.gradle.kts", ""),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["gradle"]);
        let names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, [":app", ":libs", ":libs:core", ":libs:my-util", ":tool"]);
        assert_eq!(
            g.unit_of("tools/cli/src/main/kotlin/Main.kt"),
            Some(":tool")
        );
        assert_eq!(g.unit_of("build.gradle.kts"), None, "no root build file");
        assert_eq!(
            g.allowed_from(":app").unwrap(),
            [":app", ":libs:core", ":libs:my-util", ":tool"]
                .map(String::from)
                .into()
        );
    }

    #[test]
    fn blocks_configuring_other_projects() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                ("settings.gradle", "include ':app', ':core', ':libs:a', ':libs:b'\n"),
                (
                    "build.gradle",
                    "project(':app') {\n  dependencies {\n    implementation project(':core')\n  }\n}\nsubprojects {\n  if (name != 'core') { dependencies { testImplementation(projects.libs.a) } }\n}\ndependencies { implementation project(':libs:b') }\n",
                ),
                (
                    "libs/build.gradle",
                    "subprojects { dependencies { implementation project(':core') } }\n",
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        let deps = |p: &str| -> Vec<&str> { g.deps[p].iter().map(String::as_str).collect() };
        assert_eq!(deps(":"), [":libs:b"], "the selector is no dependency");
        assert_eq!(deps(":app"), [":core", ":libs:a"]);
        assert_eq!(deps(":core"), [":libs:a"]);
        assert_eq!(deps(":libs:b"), [":core", ":libs:a"]);
        assert_eq!(deps(":libs:a"), [":core"], "itself filtered out");
    }

    #[test]
    fn groovy_dsl_with_root_project() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                ("settings.gradle", "include 'web', ':shared'\n"),
                (
                    "build.gradle",
                    "dependencies { implementation project(':shared') }\n",
                ),
                (
                    "web/build.gradle",
                    "dependencies {\n  implementation project(path: ':shared', configuration: 'default')\n}\n",
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        let names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, [":", ":web", ":shared"]);
        assert_eq!(g.unit_of("src/main/java/A.java"), Some(":"));
        assert!(g.deps[":web"].contains(":shared") && g.deps[":"].contains(":shared"));
    }
}
