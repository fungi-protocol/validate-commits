// Repositories for tests, built with git2.

use git2::build::TreeUpdateBuilder;
use git2::{FileMode, Oid, Repository, Signature, Time};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// Write a commit with a fixed date. With a reference, also point it (or
// HEAD's branch) at the new commit.
pub(crate) fn commit_as(
    repository: &Repository,
    reference: Option<&str>,
    message: &str,
    tree: Oid,
    parents: impl IntoIterator<Item = Oid>,
    timestamp: i64,
) -> Oid {
    let signature = Signature::new("Test", "test@example.com", &Time::new(timestamp, 0)).unwrap();
    let tree = repository.find_tree(tree).unwrap();
    let parents: Vec<_> = parents
        .into_iter()
        .map(|id| repository.find_commit(id).unwrap())
        .collect();
    let parents: Vec<_> = parents.iter().collect();
    repository
        .commit(reference, &signature, &signature, message, &tree, &parents)
        .unwrap()
}

pub(crate) fn empty_tree(repository: &Repository) -> Oid {
    repository.treebuilder(None).unwrap().write().unwrap()
}

// A tree with `files` written on top of `tree`.
pub(crate) fn tree_with(repository: &Repository, tree: Oid, files: &[(&str, &[u8])]) -> Oid {
    let mut update = TreeUpdateBuilder::new();
    for (path, content) in files {
        update.upsert(*path, repository.blob(content).unwrap(), FileMode::Blob);
    }
    update
        .create_updated(repository, &repository.find_tree(tree).unwrap())
        .unwrap()
}

// History built in a "remote" repository, then cloned with only a few
// commits, the way CI checks out a pull request.
pub(crate) struct Remote {
    root: TempDir,
    pub(crate) repository: Repository,
    tree: Oid,
}

impl Remote {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repository = Repository::init(root.path().join("remote")).unwrap();
        // Let clones fetch any commit by its ID, as GitHub allows.
        repository
            .config()
            .unwrap()
            .set_bool("uploadpack.allowAnySHA1InWant", true)
            .unwrap();
        let tree = repository.treebuilder(None).unwrap().write().unwrap();
        Self {
            root,
            repository,
            tree,
        }
    }

    // Add `count` commits on top of `parent`, oldest first.
    pub(crate) fn chain(&self, parent: Option<Oid>, count: usize, name: &str) -> Vec<Oid> {
        let mut commits: Vec<Oid> = Vec::new();
        for i in 0..count {
            let parent = commits.last().copied().or(parent);
            let message = format!("{name} {i}");
            commits.push(commit_as(
                &self.repository,
                None,
                &message,
                self.tree,
                parent,
                i as i64,
            ));
        }
        commits
    }

    pub(crate) fn merge(&self, parents: [Oid; 2]) -> Oid {
        commit_as(&self.repository, None, "Merge", self.tree, parents, 0)
    }

    // Clone only the newest `depth` commits of `tip`. libgit2 cannot make
    // shallow clones of local repositories, so this uses `git clone`.
    pub(crate) fn clone_shallow(&self, tip: Oid, depth: usize) -> Repository {
        self.repository
            .reference("refs/heads/queue", tip, true, "test")
            .unwrap();
        let url = format!("file://{}", self.repository.path().display());
        git(
            self.root.path(),
            &[
                "clone",
                "--quiet",
                &format!("--depth={depth}"),
                "--branch=queue",
                &url,
                "clone",
            ],
        );
        Repository::open(self.root.path().join("clone")).unwrap()
    }
}

pub(crate) fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("git must be on PATH to run the shallow clone tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
