//! Gradle provider: projects `include`d by `settings.gradle(.kts)` are units named by project
//! path (`:libs:core`; the root project, when it has a build file, is `:`); edges are
//! `project(":x")` references and type-safe `projects.x` accessors in a project's build file.
//! Both DSLs are scanned with regexes, not evaluated: only literal paths are understood.

use std::collections::BTreeMap;
use std::fs;
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
                let dir = match dirs.get(&path) {
                    Some(d) => workspace::join(root, d),
                    None => root.join(path.trim_start_matches(':').replace(':', "/")),
                };
                projects.push((path, dir));
            }
        }
        let accessors: BTreeMap<String, String> = projects
            .iter()
            .map(|(p, _)| (accessor(p), p.clone()))
            .collect();
        let packages = projects
            .into_iter()
            .filter_map(|(path, dir)| {
                let script = read_script(&dir, BUILD);
                // The root is a unit only with a build file of its own.
                if path == ":" && script.is_none() {
                    return None;
                }
                let (manifest, text) =
                    script.unwrap_or_else(|| (dir.join(BUILD[1]), String::new()));
                let mut pkg = Package::new(path, &manifest);
                pkg.deps
                    .extend(PROJECT_REF.captures_iter(&text).map(|c| c[1].to_string()));
                pkg.deps.extend(
                    ACCESSOR
                        .captures_iter(&text)
                        .filter_map(|c| accessors.get(&c[1]).cloned()),
                );
                Some(pkg)
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
        assert_eq!(names, [":app", ":libs:core", ":libs:my-util", ":tool"]);
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
