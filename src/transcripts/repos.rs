//! Which registered repository a call worked in, from a path or a `repo` argument. A linked
//! worktree counts towards its repository's registered main checkout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::registry;

use super::Target;

pub struct Repos {
    names: HashMap<String, PathBuf>,
    roots: Vec<PathBuf>,
    /// A repository's shared git directory → its registered root, the main checkout preferred.
    by_git: HashMap<PathBuf, PathBuf>,
    /// A registered linked worktree → its repository's registered root, by its name
    /// (`repo:worktree`), for a worktree that has since been removed.
    main_of: HashMap<PathBuf, PathBuf>,
    /// Directory → repository, as resolved so far.
    seen: RefCell<HashMap<PathBuf, Option<PathBuf>>>,
}

impl Repos {
    pub fn new(config: &Config) -> Self {
        let registered = registry::registered(config);
        let mut by_git: HashMap<PathBuf, (PathBuf, bool)> = HashMap::new();
        for (_, root) in &registered {
            if let Some((git, linked)) = registry::git_repository(root) {
                let better = by_git.get(&git).is_none_or(|(_, l)| *l && !linked);
                if better {
                    by_git.insert(git, (root.clone(), linked));
                }
            }
        }
        let names: HashMap<String, PathBuf> = registered.iter().cloned().collect();
        let main_of = registered
            .iter()
            .filter_map(|(name, root)| {
                let main = names.get(name.split_once(':')?.0)?;
                Some((root.clone(), main.clone()))
            })
            .collect();
        Self {
            main_of,
            names,
            roots: registered.into_iter().map(|(_, r)| r).collect(),
            by_git: by_git.into_iter().map(|(g, (r, _))| (g, r)).collect(),
            seen: RefCell::new(HashMap::new()),
        }
    }

    /// Identifies the registered repositories: a git repository by its shared git directory, so
    /// that registering another worktree of one does not change it.
    pub fn fingerprint(&self) -> String {
        let mut ids: Vec<String> = self
            .roots
            .iter()
            .map(|r| {
                registry::git_repository(r)
                    .map_or_else(|| r.clone(), |(git, _)| git)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids.join("\n")
    }

    pub fn of(&self, t: &Target) -> Option<PathBuf> {
        match t {
            Target::Repo(r) => {
                self.of_path(self.names.get(r).map_or(Path::new(r), PathBuf::as_path))
            }
            Target::Path(p) => self.of_path(p),
        }
    }

    /// The repository a file or directory is in.
    pub fn of_path(&self, path: &Path) -> Option<PathBuf> {
        let dir = if path.is_file() { path.parent()? } else { path };
        if let Some(r) = self.seen.borrow().get(dir) {
            return r.clone();
        }
        let repo = registry::git_repository(dir)
            .and_then(|(git, _)| self.by_git.get(&git).cloned())
            .or_else(|| registry::root_of(&self.roots, dir))
            .map(|root| self.main_of.get(&root).cloned().unwrap_or(root));
        self.seen
            .borrow_mut()
            .insert(dir.to_path_buf(), repo.clone());
        repo
    }
}
