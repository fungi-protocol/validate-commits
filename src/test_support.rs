// Repositories for tests, built with gix.

use gix::refs::transaction::PreviousValue;
use gix::{ObjectId, Repository};

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
