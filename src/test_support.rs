// Repositories for tests, built with gix.

use gix::refs::transaction::PreviousValue;
use gix::{ObjectId, Repository};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// Fixtures update branches, which writes a reflog entry naming a
// committer. Set one in memory so the tests also run where Git has no
// user config, like the Nix build sandbox.
pub(crate) fn with_test_identity(mut repository: Repository) -> Repository {
    let mut config = repository.config_snapshot_mut();
    config
        .set_value(&gix::config::tree::Committer::NAME, "Test")
        .unwrap();
    config
        .set_value(&gix::config::tree::Committer::EMAIL, "test@example.com")
        .unwrap();
    config.commit().unwrap();
    repository
}

// Write a commit with a fixed date. With a reference, also point it (or
// HEAD's branch) at the new commit.
pub(crate) fn commit_as(
    repository: &Repository,
    reference: Option<&str>,
    message: &str,
    tree: ObjectId,
    parents: impl IntoIterator<Item = ObjectId>,
    timestamp: i64,
) -> ObjectId {
    let signature = gix::actor::Signature {
        name: "Test".into(),
        email: "test@example.com".into(),
        time: gix::date::Time::new(timestamp, 0),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: parents.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    let id = repository.write_object(&commit).unwrap().detach();
    if let Some(reference) = reference {
        let name = match reference {
            "HEAD" => repository.head_name().unwrap().unwrap(),
            name => name.try_into().unwrap(),
        };
        repository
            .reference(name, id, PreviousValue::Any, "test")
            .unwrap();
    }
    id
}

// History built in a "remote" repository, then cloned with only a few
// commits, the way CI checks out a pull request.
pub(crate) struct Remote {
    root: TempDir,
    pub(crate) repository: Repository,
    tree: ObjectId,
}

impl Remote {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repository = with_test_identity(gix::init(root.path().join("remote")).unwrap());
        // Let clones fetch any commit by its ID, as GitHub allows.
        git(
            repository.workdir().unwrap(),
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let tree = repository.empty_tree().id;
        Self {
            root,
            repository,
            tree,
        }
    }

    // Add `count` commits on top of `parent`, oldest first.
    pub(crate) fn chain(
        &self,
        parent: Option<ObjectId>,
        count: usize,
        name: &str,
    ) -> Vec<ObjectId> {
        let mut commits: Vec<ObjectId> = Vec::new();
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

    pub(crate) fn merge(&self, parents: [ObjectId; 2]) -> ObjectId {
        commit_as(&self.repository, None, "Merge", self.tree, parents, 0)
    }

    // Clone only the newest `depth` commits of `tip`.
    pub(crate) fn clone_shallow(&self, tip: ObjectId, depth: usize) -> Repository {
        self.repository
            .reference("refs/heads/queue", tip, PreviousValue::Any, "test")
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
        with_test_identity(gix::open(self.root.path().join("clone")).unwrap())
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
