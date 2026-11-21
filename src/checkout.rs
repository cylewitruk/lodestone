//! Git checkouts: which one a directory is in, and which nested checkouts an index leaves to their
//! own index. Request routing ([`crate::registry::place`]) and the index walk share these rules.

use std::fs;
use std::path::{Path, PathBuf};

/// The git directory of a worktree: `.git/` or the `gitdir:` target of a `.git` file, resolved
/// (a relative `../repo.git` is outside the worktree, though it starts with its path) when it
/// exists.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = fs::read_to_string(&dot).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let p = root.join(target);
    Some(p.canonicalize().unwrap_or(p))
}

/// The git checkout a directory is in.
pub struct Checkout {
    /// Its top-level directory: the nearest one holding `.git`.
    pub top: PathBuf,
    /// Its own git directory (`.git`, `.git/worktrees/<name>`, `.git/modules/<name>`).
    pub git_dir: PathBuf,
    /// The repository's shared git directory, the same for all of its worktrees.
    pub common: PathBuf,
}

impl Checkout {
    pub fn of(dir: &Path) -> Option<Self> {
        let top = dir.ancestors().find(|d| d.join(".git").exists())?;
        let git_dir = git_dir(top)?.canonicalize().ok()?;
        let common = match fs::read_to_string(git_dir.join("commondir")) {
            Ok(rel) => git_dir.join(rel.trim()).canonicalize().ok()?,
            Err(_) => git_dir.clone(),
        };
        Some(Self {
            top: top.to_path_buf(),
            git_dir,
            common,
        })
    }

    /// A worktree added with `git worktree add`, rather than a repository's main checkout.
    pub fn is_linked(&self) -> bool {
        self.git_dir != self.common
    }

    /// Whether this checkout, nested inside `outer`'s tree, is served by its own index: a linked
    /// worktree, or a separate repository when `outer` is a git checkout. A submodule belongs to
    /// `outer`, and so does any checkout inside a directory that is not in one.
    pub fn is_separate_from(&self, outer: Option<&Checkout>) -> bool {
        self.is_linked() || outer.is_some_and(|o| !self.git_dir.starts_with(&o.common))
    }
}

/// Whether `dir`, a directory below an index's root, holds a checkout served by its own index
/// (see [`Checkout::is_separate_from`]); `outer` is the checkout the root is in.
pub fn is_separate_checkout(dir: &Path, outer: Option<&Checkout>) -> bool {
    dir.join(".git").exists() && Checkout::of(dir).is_some_and(|c| c.is_separate_from(outer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_file_worktree() {
        let root = crate::testutil::tempdir();
        let target = crate::testutil::tempdir().join(".git/worktrees/wt");
        fs::write(root.join(".git"), format!("gitdir: {}\n", target.display())).unwrap();
        assert_eq!(git_dir(&root), Some(target));
    }

    #[test]
    fn a_relative_git_dir_is_resolved() {
        let base = crate::testutil::tempdir().canonicalize().unwrap();
        fs::create_dir_all(base.join("external.git")).unwrap();
        fs::create_dir_all(base.join("wt")).unwrap();
        fs::write(base.join("wt/.git"), "gitdir: ../external.git\n").unwrap();
        assert_eq!(git_dir(&base.join("wt")), Some(base.join("external.git")));
    }

    #[test]
    fn nested_repositories_are_separate_and_submodules_are_not() {
        let root = crate::testutil::tempdir().canonicalize().unwrap();
        fs::create_dir_all(root.join(".git/modules/sub")).unwrap();
        fs::create_dir_all(root.join("other/.git")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        let outer = Checkout::of(&root);
        assert!(is_separate_checkout(&root.join("other"), outer.as_ref()));
        assert!(!is_separate_checkout(&root.join("sub"), outer.as_ref()));
        assert!(!is_separate_checkout(&root.join("plain"), outer.as_ref()));
        // A directory outside any checkout serves the repositories inside it.
        assert!(!is_separate_checkout(&root.join("other"), None));
    }
}
