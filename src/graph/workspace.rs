//! Helpers shared by providers: member discovery, manifest reading and unit assembly.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobMatcher, GlobSetBuilder};
use roxmltree::{Document, Node};
use toml::{Table, Value};

use super::{Edges, Unit};

/// A package a provider found.
#[derive(Debug, Default)]
pub struct Package {
    pub name: String,
    /// Its manifest; the manifest's directory is the unit's.
    pub manifest: PathBuf,
    /// Names of packages it depends on.
    pub deps: BTreeSet<String>,
    /// Manifests (or their directories) of packages it depends on by path.
    pub dep_paths: Vec<PathBuf>,
    /// Manifests it lists as members: visited by [`collect`], not dependencies.
    pub members: Vec<PathBuf>,
}

impl Package {
    pub fn new(name: impl Into<String>, manifest: &Path) -> Self {
        Self {
            name: name.into(),
            manifest: manifest.to_path_buf(),
            ..Self::default()
        }
    }

    pub fn dir(&self) -> &Path {
        self.manifest.parent().unwrap_or(&self.manifest)
    }
}

pub fn read_toml(path: &Path) -> Option<Table> {
    fs::read_to_string(path).ok()?.parse::<Table>().ok()
}

/// `f` applied to the root element of the XML document at `path`.
pub fn read_xml<T>(path: &Path, f: impl FnOnce(Node) -> T) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    let doc = Document::parse(&text).ok()?;
    Some(f(doc.root_element()))
}

/// Element children of `node` with local name `tag`.
pub fn xml_children<'a, 'i>(node: Node<'a, 'i>, tag: &str) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

/// The strings of a TOML array (non-strings skipped); empty when `value` is not an array.
pub fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// `path` with `.` and `..` resolved lexically.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `path` relative to `root`, `/`-separated (`""` for the root itself).
pub fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// `dir` joined with a manifest-relative path that may use `\` separators.
pub fn join(dir: &Path, path: &str) -> PathBuf {
    normalize(&dir.join(path.trim().replace('\\', "/")))
}

/// Subdirectories of `dir`, skipping hidden ones and `node_modules`.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "node_modules"
        })
        .map(|e| e.path())
        .collect()
}

fn descendants(dir: &Path, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    for d in subdirs(dir) {
        descendants(&d, out);
    }
}

/// Directories under `root` matching a `/`-separated glob: a segment may hold glob syntax
/// (`*`, `?`, `[..]`, `{a,b}`) or be `**`; wildcards skip hidden directories and `node_modules`.
pub fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for seg in pattern.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let matcher: Option<GlobMatcher> = (seg != "**" && seg.contains(['*', '?', '[', '{']))
            .then(|| Glob::new(seg).ok().map(|g| g.compile_matcher()))
            .flatten();
        let mut next = Vec::new();
        for d in &dirs {
            if seg == "**" {
                descendants(d, &mut next);
            } else if let Some(m) = &matcher {
                next.extend(
                    subdirs(d)
                        .into_iter()
                        .filter(|p| p.file_name().is_some_and(|n| m.is_match(n))),
                );
            } else {
                next.push(d.join(seg));
            }
        }
        dirs = next;
    }
    let dirs: BTreeSet<PathBuf> = dirs.iter().map(|d| normalize(d)).collect();
    dirs.into_iter().collect()
}

/// Workspace member directories: those matching any pattern, minus those matching a pattern
/// prefixed with `!`, kept when `is_member` accepts them.
pub fn members<S: AsRef<str>>(
    root: &Path,
    patterns: impl IntoIterator<Item = S>,
    is_member: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let clean = |p: &str| {
        p.trim()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    };
    let mut includes = Vec::new();
    let mut excludes = GlobSetBuilder::new();
    for p in patterns {
        match p.as_ref().trim().strip_prefix('!') {
            Some(ex) => {
                if let Ok(g) = Glob::new(&clean(ex)) {
                    excludes.add(g);
                }
            }
            None => includes.push(clean(p.as_ref())),
        }
    }
    let excludes = excludes.build().unwrap_or_default();
    let mut out: Vec<PathBuf> = Vec::new();
    for pattern in includes {
        for d in expand(root, &pattern) {
            if !excludes.is_match(rel(root, &d)) && is_member(&d) && !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// The root and the directories up to two levels below it: where unlisted packages are looked
/// for when no workspace file lists them.
pub fn nearby(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    out.extend(expand(root, "*"));
    out.extend(expand(root, "*/*"));
    out
}

/// Packages read from `seeds` (manifest paths) and, transitively, the manifests their
/// `dep_paths` and `members` name inside `root`. A directory stands for its manifest when
/// `read` resolves it so.
pub fn collect(
    root: &Path,
    seeds: impl IntoIterator<Item = PathBuf>,
    mut read: impl FnMut(&Path) -> Option<Package>,
) -> Vec<Package> {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<PathBuf> = seeds.into_iter().collect();
    let mut out = Vec::new();
    while let Some(m) = queue.pop_front() {
        let m = normalize(&m);
        if !m.starts_with(root) || !seen.insert(m.clone()) {
            continue;
        }
        let Some(p) = read(&m) else { continue };
        queue.extend(p.dep_paths.iter().chain(&p.members).cloned());
        out.push(p);
    }
    out
}

/// Units and edges for `packages`: a dependency is an edge when it names, or points at, another
/// package. Packages sharing a name are one unit with several paths. `None` when there are none.
pub fn graph(provider: &str, root: &Path, packages: Vec<Package>) -> Option<(Vec<Unit>, Edges)> {
    if packages.is_empty() {
        return None;
    }
    let names: BTreeSet<&str> = packages.iter().map(|p| p.name.as_str()).collect();
    let by_path: BTreeMap<PathBuf, &str> = packages
        .iter()
        .flat_map(|p| {
            [
                (normalize(&p.manifest), p.name.as_str()),
                (normalize(p.dir()), p.name.as_str()),
            ]
        })
        .collect();
    let mut deps = Edges::new();
    let mut units: Vec<Unit> = Vec::new();
    for p in &packages {
        let edges = p
            .deps
            .iter()
            .map(String::as_str)
            .chain(
                p.dep_paths
                    .iter()
                    .filter_map(|d| by_path.get(&normalize(d)).copied()),
            )
            .filter(|d| names.contains(d) && *d != p.name)
            .map(String::from);
        deps.entry(p.name.clone()).or_default().extend(edges);
        let path = rel(root, p.dir());
        match units.iter_mut().find(|u| u.name == p.name) {
            Some(u) if !u.paths.contains(&path) => u.paths.push(path),
            Some(_) => {}
            None => units.push(Unit {
                name: p.name.clone(),
                paths: vec![path],
                provider: provider.into(),
            }),
        }
    }
    Some((units, deps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{tempdir, write_tree};

    #[test]
    fn expand_and_members() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                ("packages/a/m", ""),
                ("packages/b/m", ""),
                ("packages/test/c/m", ""),
                ("packages/node_modules/x/m", ""),
                ("packages/.hidden/m", ""),
                ("libs/x-core/m", ""),
                ("libs/x-util/n", ""),
            ],
        );
        let names =
            |dirs: Vec<PathBuf>| -> Vec<String> { dirs.iter().map(|d| rel(&root, d)).collect() };
        assert_eq!(
            names(expand(&root, "packages/*")),
            ["packages/a", "packages/b", "packages/test"]
        );
        assert_eq!(
            names(expand(&root, "./libs/x-*/")),
            ["libs/x-core", "libs/x-util"]
        );
        let has_m = |d: &Path| d.join("m").is_file();
        assert_eq!(
            names(members(
                &root,
                ["packages/**", "libs/*", "!**/test/**"],
                has_m
            )),
            ["packages/a", "packages/b", "libs/x-core"]
        );
        assert_eq!(
            names(members(&root, ["libs/*", "!libs/x-core"], |_| true)),
            ["libs/x-util"]
        );
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn graph_merges_names_and_resolves_paths() {
        let root = PathBuf::from("/r");
        let mut a = Package::new("a", &root.join("a/m"));
        a.dep_paths.push(root.join("x/../b"));
        a.deps.insert("ext".into());
        let b = Package::new("b", &root.join("b/m"));
        let b2 = Package::new("b", &root.join("b2/m"));
        let (units, deps) = graph("t", &root, vec![a, b, b2]).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[1].paths, ["b", "b2"]);
        assert_eq!(deps["a"], BTreeSet::from(["b".to_string()]));
        assert!(graph("t", &root, Vec::new()).is_none());
    }
}
