//! .NET provider: C#, F# and VB projects are units named by project file stem; edges are
//! `<ProjectReference>`s. Projects come from the root's `.sln` / `.slnx` files, else from
//! project files in the root and up to two levels below it; references are followed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::workspace::{self, Package, xml_children};
use super::{Edges, ProjectGraphProvider, Unit};

pub struct DotnetProvider;

const PROJECT_EXTENSIONS: &[&str] = &["csproj", "fsproj", "vbproj"];

/// `Project("{type}") = "Name", "path\to\Name.csproj", "{guid}"`.
static SLN_PROJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*Project\("[^"]*"\)\s*=\s*"[^"]*"\s*,\s*"([^"]+)""#).unwrap()
});

fn is_project(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PROJECT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Files in `dir` that `keep` accepts, sorted.
fn files_with(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && keep(p))
        .collect();
    out.sort();
    out
}

/// Project files a solution lists.
fn solution_projects(sln: &Path) -> Vec<PathBuf> {
    let dir = sln.parent().unwrap_or(sln);
    let paths: Vec<String> = if sln.extension().is_some_and(|e| e == "slnx") {
        workspace::read_xml(sln, |s| {
            s.descendants()
                .filter(|n| n.has_tag_name("Project"))
                .filter_map(|n| n.attribute("Path").map(String::from))
                .collect()
        })
        .unwrap_or_default()
    } else {
        let text = fs::read_to_string(sln).unwrap_or_default();
        SLN_PROJECT
            .captures_iter(&text)
            .map(|c| c[1].to_string())
            .collect()
    };
    paths
        .iter()
        .map(|p| workspace::join(dir, p))
        .filter(|p| is_project(p))
        .collect()
}

fn read(manifest: &Path) -> Option<Package> {
    let dir = manifest.parent()?;
    let name = manifest.file_stem()?.to_string_lossy();
    workspace::read_xml(manifest, |project| {
        let mut pkg = Package::new(name, manifest);
        pkg.dep_paths = xml_children(project, "ItemGroup")
            .flat_map(|g| xml_children(g, "ProjectReference"))
            .filter_map(|r| r.attribute("Include"))
            .map(|p| workspace::join(dir, p))
            .collect();
        pkg
    })
}

impl ProjectGraphProvider for DotnetProvider {
    fn name(&self) -> &'static str {
        "dotnet"
    }

    fn detect(&self, root: &Path) -> Option<(Vec<Unit>, Edges)> {
        let solutions = files_with(root, |p| {
            p.extension().is_some_and(|e| e == "sln" || e == "slnx")
        });
        let seeds: Vec<PathBuf> = if solutions.is_empty() {
            workspace::nearby(root)
                .iter()
                .flat_map(|d| files_with(d, is_project))
                .collect()
        } else {
            solutions
                .iter()
                .flat_map(|s| solution_projects(s))
                .collect()
        };
        let packages = workspace::collect(root, seeds, read);
        workspace::graph(self.name(), root, packages)
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::ProjectGraph;
    use crate::testutil::{tempdir, write_tree};

    const SLN: &str = r#"
Microsoft Visual Studio Solution File, Format Version 12.00
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "src", "src", "{11111111-0000-0000-0000-000000000000}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Acme.Api", "src\Acme.Api\Acme.Api.csproj", "{22222222-0000-0000-0000-000000000000}"
EndProject
"#;

    #[test]
    fn solution_and_project_references() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                ("Acme.sln", SLN),
                (
                    "src/Acme.Api/Acme.Api.csproj",
                    r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <ItemGroup>
    <ProjectReference Include="..\Acme.Core\Acme.Core.csproj" />
    <PackageReference Include="Serilog" Version="4.0.0" />
  </ItemGroup>
</Project>"#,
                ),
                (
                    "src/Acme.Core/Acme.Core.csproj",
                    r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup><ProjectReference Include="../../lib/Acme.Text/Acme.Text.fsproj" /></ItemGroup></Project>"#,
                ),
                (
                    "lib/Acme.Text/Acme.Text.fsproj",
                    r#"<Project Sdk="Microsoft.NET.Sdk" />"#,
                ),
                (
                    "samples/Demo/Demo.csproj",
                    r#"<Project Sdk="Microsoft.NET.Sdk" />"#,
                ),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert_eq!(g.providers, ["dotnet"]);
        let names: Vec<&str> = g.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            ["Acme.Api", "Acme.Core", "Acme.Text"],
            "the solution's, and references"
        );
        assert_eq!(g.unit_of("lib/Acme.Text/Library.fs"), Some("Acme.Text"));
        assert_eq!(
            g.allowed_from("Acme.Api").unwrap(),
            ["Acme.Api", "Acme.Core", "Acme.Text"]
                .map(String::from)
                .into()
        );
    }

    #[test]
    fn slnx_and_unlisted_projects() {
        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "App.slnx",
                    r#"<Solution><Folder Name="/src/"><Project Path="src/App/App.vbproj" /></Folder></Solution>"#,
                ),
                ("src/App/App.vbproj", "<Project />"),
            ],
        );
        assert_eq!(ProjectGraph::build(&root).units.len(), 1);

        let root = tempdir();
        write_tree(
            &root,
            &[
                (
                    "src/App/App.csproj",
                    r#"<Project><ItemGroup><ProjectReference Include="..\Lib\Lib.csproj" /></ItemGroup></Project>"#,
                ),
                ("src/Lib/Lib.csproj", "<Project />"),
            ],
        );
        let g = ProjectGraph::build(&root);
        assert!(g.deps["App"].contains("Lib"));
    }
}
