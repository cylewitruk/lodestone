//! Maven provider: the root `pom.xml` and its `<modules>`, recursively, are units named by
//! `artifactId`; edges are `<dependency>` entries naming a sibling module's `artifactId`.
//! Profiles' modules and dependencies count; `<dependencyManagement>` and plugins do not.

use std::path::Path;

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

fn read(manifest: &Path) -> Option<Package> {
    let dir = manifest.parent()?;
    workspace::read_xml(manifest, |project| {
        let mut pkg = Package::new(text_of(project, "artifactId")?, manifest);
        // The project and each profile hold modules and dependencies alike.
        let scopes = xml_children(project, "profiles")
            .flat_map(|ps| xml_children(ps, "profile"))
            .chain([project]);
        for scope in scopes {
            for module in xml_children(scope, "modules").flat_map(|m| xml_children(m, "module")) {
                let Some(path) = module.text().map(str::trim) else {
                    continue;
                };
                let path = workspace::join(dir, path);
                pkg.members.push(match path.extension() {
                    Some(ext) if ext == "xml" => path,
                    _ => path.join("pom.xml"),
                });
            }
            pkg.deps.extend(
                xml_children(scope, "dependencies")
                    .flat_map(|d| xml_children(d, "dependency"))
                    .filter_map(|d| text_of(d, "artifactId"))
                    .map(String::from),
            );
        }
        Some(pkg)
    })
    .flatten()
}

impl ProjectGraphProvider for MavenProvider {
    fn name(&self) -> &'static str {
        "maven"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let packages = workspace::collect(root, [root.join("pom.xml")], read);
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
}
