//! Maven provider: the root `pom.xml` and its `<modules>`, recursively, are units named by
//! `artifactId`; edges are `<dependency>` entries naming a sibling module's `artifactId`.
//! Profiles' modules and dependencies count, and so do those inherited from a local
//! `<parent>`; `<dependencyManagement>` and plugins do not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use roxmltree::Node;

use super::workspace::{self, Package, xml_children};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct MavenProvider;

fn text_of<'a>(node: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    xml_children(node, tag)
        .next()
        .and_then(|n| n.text())
        .map(str::trim)
}

/// A `<module>` or `<relativePath>` resolved against `dir`: a POM file, or a directory's `pom.xml`.
fn pom_at(dir: &Path, path: &str) -> PathBuf {
    let path = workspace::join(dir, path);
    match path.extension() {
        Some(ext) if ext == "xml" => path,
        _ => path.join("pom.xml"),
    }
}

/// A POM's `<parent>`: its `artifactId`, and the POM `<relativePath>` names (`../pom.xml` by
/// default; `None` when empty, which means the parent comes from a repository).
struct Parent {
    artifact_id: String,
    pom: Option<PathBuf>,
}

fn read(manifest: &Path) -> Option<(Package, Option<Parent>)> {
    let dir = manifest.parent()?;
    workspace::read_xml(manifest, |project| {
        let mut pkg = Package::new(text_of(project, "artifactId")?, manifest);
        // The project and each profile hold modules and dependencies alike.
        let scopes = xml_children(project, "profiles")
            .flat_map(|ps| xml_children(ps, "profile"))
            .chain([project]);
        for scope in scopes {
            for module in xml_children(scope, "modules").flat_map(|m| xml_children(m, "module")) {
                if let Some(path) = module.text() {
                    pkg.members.push(pom_at(dir, path));
                }
            }
            pkg.deps.extend(
                xml_children(scope, "dependencies")
                    .flat_map(|d| xml_children(d, "dependency"))
                    .filter_map(|d| text_of(d, "artifactId"))
                    .map(String::from),
            );
        }
        let parent = xml_children(project, "parent").next().and_then(|p| {
            let pom = match xml_children(p, "relativePath").next() {
                None => Some(pom_at(dir, "..")),
                Some(r) => r.text().map(str::trim).filter(|t| !t.is_empty()).map(|t| pom_at(dir, t)),
            };
            Some(Parent {
                artifact_id: text_of(p, "artifactId")?.to_string(),
                pom,
            })
        });
        Some((pkg, parent))
    })
    .flatten()
}

/// Adds to each package the `<dependencies>` of its local parents, transitively. A parent is the
/// package `<relativePath>` names, if its `artifactId` matches, else the package with that
/// `artifactId`.
fn inherit(packages: &mut [Package], parents: &BTreeMap<PathBuf, Parent>) {
    let parent: Vec<Option<usize>> = packages
        .iter()
        .map(|pkg| {
            let p = parents.get(&pkg.manifest)?;
            let named = |q: &&Package| q.name == p.artifact_id;
            packages
                .iter()
                .position(|q| named(&q) && p.pom.as_ref() == Some(&q.manifest))
                .or_else(|| packages.iter().position(|q| named(&q)))
        })
        .collect();
    let own: Vec<BTreeSet<String>> = packages.iter().map(|p| p.deps.clone()).collect();
    for (i, pkg) in packages.iter_mut().enumerate() {
        let mut seen = BTreeSet::from([i]);
        let mut next = parent[i];
        while let Some(j) = next.filter(|j| seen.insert(*j)) {
            pkg.deps.extend(own[j].iter().cloned());
            next = parent[j];
        }
    }
}

impl ProjectGraphProvider for MavenProvider {
    fn name(&self) -> &'static str {
        "maven"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let mut parents = BTreeMap::new();
        let mut packages = workspace::collect(root, [root.join("pom.xml")], |m| {
            let (pkg, parent) = read(m)?;
            parents.extend(parent.map(|p| (m.to_path_buf(), p)));
            Some(pkg)
        });
        inherit(&mut packages, &parents);
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    const HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent><groupId>com.acme</groupId><artifactId>parent</artifactId><version>1</version></parent>"#;

    #[test]
    fn multi_module() {
        let root = tempdir();
        let pom = |body: &str| format!("{HEAD}\n{body}\n</project>\n");
        write_tree(
            &root,
            &[
                (
                    "pom.xml",
                    &pom(
                        "<artifactId>parent</artifactId><packaging>pom</packaging>\n<modules><module>app</module><module>libs</module></modules>\n<profiles><profile><modules><module>extra/pom.xml</module></modules></profile></profiles>",
                    ),
                ),
                (
                    "app/pom.xml",
                    &pom(
                        "<artifactId>app</artifactId>\n<dependencies><dependency><groupId>com.acme</groupId><artifactId>core</artifactId></dependency><dependency><artifactId>junit</artifactId></dependency></dependencies>",
                    ),
                ),
                (
                    "libs/pom.xml",
                    &pom("<artifactId>libs</artifactId><modules><module>./core</module></modules>"),
                ),
                (
                    "libs/core/pom.xml",
                    &pom(
                        "<artifactId>core</artifactId>\n<dependencyManagement><dependencies><dependency><artifactId>app</artifactId></dependency></dependencies></dependencyManagement>\n<!-- <dependencies><dependency><artifactId>extra</artifactId></dependency></dependencies> -->",
                    ),
                ),
                ("extra/pom.xml", &pom("<artifactId>extra</artifactId>")),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["maven"]);
        let mut names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["app", "core", "extra", "libs", "parent"]);
        assert_eq!(g.unit_of("libs/core/src/main/java/A.java"), Some("core"));
        assert_eq!(
            g.allowed_from("app").unwrap(),
            ["app", "core"].map(String::from).into()
        );
        assert!(g.deps["core"].is_empty());
    }

    #[test]
    fn inherited_dependencies() {
        let root = tempdir();
        let pom = |parent: &str, body: &str| {
            format!("<project><parent><artifactId>{parent}</artifactId>{body}</project>")
        };
        write_tree(
            &root,
            &[
                (
                    "pom.xml",
                    "<project><artifactId>root</artifactId><modules><module>core</module><module>extra</module><module>libs</module><module>apps/web</module></modules>\n<dependencies><dependency><artifactId>core</artifactId></dependency></dependencies>\n<dependencyManagement><dependencies><dependency><artifactId>extra</artifactId></dependency></dependencies></dependencyManagement></project>",
                ),
                ("core/pom.xml", &pom("root", "</parent><artifactId>core</artifactId>")),
                ("extra/pom.xml", &pom("root", "</parent><artifactId>extra</artifactId>")),
                // Default `relativePath`: `../pom.xml`.
                (
                    "libs/pom.xml",
                    &pom("root", "</parent><artifactId>libs</artifactId><modules><module>a</module></modules>"),
                ),
                ("libs/a/pom.xml", &pom("libs", "</parent><artifactId>a</artifactId>")),
                // An empty `relativePath`: found by `artifactId`.
                (
                    "apps/web/pom.xml",
                    &pom("root", "<relativePath/></parent><artifactId>web</artifactId>"),
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        let deps = |p: &str| -> Vec<&str> { g.deps[p].iter().map(String::as_str).collect() };
        assert_eq!(deps("libs"), ["core"]);
        assert_eq!(deps("a"), ["core"], "through libs");
        assert_eq!(deps("web"), ["core"]);
        assert!(deps("core").is_empty(), "not itself");
        assert!(!g.allowed_from("a").unwrap().contains("extra"));
    }
}
