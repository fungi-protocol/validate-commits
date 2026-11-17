use git2::{Oid, Repository, RevparseMode, Sort};

use crate::{CommitId, Error};

// Find the commits to validate: reachable from an included revision but
// not from an excluded one, with parents before their children.
pub(crate) fn resolve_commits(
    repo: &Repository,
    revisions: &[&str],
) -> Result<Vec<CommitId>, Error> {
    let parsed = parse_revisions(repo, revisions)?;
    let selected = walk(repo, &parsed.include, &parsed.exclude)?;
    Ok(selected.into_iter().map(CommitId).collect())
}

// The commits named by the arguments, split into included and excluded.
struct Revisions {
    include: Vec<Oid>,
    exclude: Vec<Oid>,
}

// Accept the two forms libgit2's revision parser returns that only include
// or exclude history: X and A..B. A...B needs common ancestors, which a
// shallow clone may not have downloaded, so it is rejected in every clone.
fn parse_revisions(repo: &Repository, revisions: &[&str]) -> Result<Revisions, Error> {
    if revisions.is_empty() {
        return Err(Error::NoRevisions);
    }
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for input in revisions {
        let spec = repo.revparse(input).map_err(Error::ParseRevision)?;
        let commit = |object: Option<&git2::Object<'_>>| {
            object
                .expect("libgit2 sets both ends of a range")
                .peel_to_commit()
                .map(|commit| commit.id())
                .map_err(Error::PeelToCommit)
        };
        let mode = spec.mode();
        if mode.contains(RevparseMode::MERGE_BASE) {
            return Err(Error::UnsupportedRevision);
        } else if mode.contains(RevparseMode::RANGE) {
            exclude.push(commit(spec.from())?);
            include.push(commit(spec.to())?);
        } else {
            include.push(commit(spec.from())?);
        }
    }
    Ok(Revisions { include, exclude })
}

// The selected commits, parents first.
fn walk(repo: &Repository, include: &[Oid], exclude: &[Oid]) -> Result<Vec<Oid>, Error> {
    let mut walk = repo.revwalk().map_err(Error::WalkHistory)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
        .map_err(Error::WalkHistory)?;
    for id in include {
        walk.push(*id).map_err(Error::WalkHistory)?;
    }
    for id in exclude {
        walk.hide(*id).map_err(Error::WalkHistory)?;
    }
    walk.map(|id| id.map_err(Error::WalkHistory)).collect()
}

#[cfg(test)]
mod tests {
    use super::{Oid, Repository, resolve_commits};
    use crate::Error;
    use tempfile::TempDir;

    struct History {
        _root: TempDir,
        repository: Repository,
        commits: Vec<Oid>,
    }

    impl History {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let repository = Repository::init(root.path()).unwrap();
            let tree = repository.treebuilder(None).unwrap().write().unwrap();
            let mut commits = Vec::new();
            for (timestamp, message) in ["Base", "First", "Second", "Third"].into_iter().enumerate()
            {
                commits.push(commit_as(
                    &repository,
                    Some("HEAD"),
                    message,
                    tree,
                    commits.last().copied(),
                    timestamp as i64,
                ));
            }
            Self {
                _root: root,
                repository,
                commits,
            }
        }
    }

    fn signature(timestamp: i64) -> git2::Signature<'static> {
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(timestamp, 0)).unwrap()
    }

    fn commit_as(
        repository: &Repository,
        reference: Option<&str>,
        message: &str,
        tree: Oid,
        parents: impl IntoIterator<Item = Oid>,
        timestamp: i64,
    ) -> Oid {
        let signature = signature(timestamp);
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

    fn select(fixture: &History, revisions: &[&str]) -> Result<Vec<Oid>, Error> {
        resolve_commits(&fixture.repository, revisions)
            .map(|commits| commits.into_iter().map(|commit| commit.0).collect())
    }

    fn assert_selection(mut actual: Vec<Oid>, expected: &[Oid]) {
        actual.sort();
        let mut expected = expected.to_vec();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn orders_merge_parents_with_skewed_dates() {
        let fixture = History::new();
        let base = fixture.commits[3];
        let tree = fixture.repository.find_commit(base).unwrap().tree_id();
        let repository = &fixture.repository;
        let commit = |message, parents: &[Oid], timestamp| {
            commit_as(
                repository,
                None,
                message,
                tree,
                parents.iter().copied(),
                timestamp,
            )
        };
        // Fork is Left's parent, but Fork has the newest date and Left the
        // oldest. Sorting by date would check Left before Fork. A parent must
        // always be checked before its children, whatever the dates say.
        let fork = commit("Fork", &[base], 900);
        let left = commit("Left", &[fork], 10);
        let a = commit("A", &[left], 100);
        let b = commit("B", &[a], 100);
        let head = commit("Merge", &[b, fork], 100);

        let commits = select(&fixture, &[&format!("{base}..{head}")]).unwrap();
        assert_eq!(commits, vec![fork, left, a, b, head]);
    }

    #[test]
    fn combines_multiple_revisions_and_ranges() {
        // History: Base -> First -> Second -> Third (HEAD), plus a "side"
        // branch that also starts from Base. Several arguments select the
        // union of what each one selects, like `git log`.
        let fixture = History::new();
        let base = fixture.commits[0];
        let tree = fixture.repository.find_commit(base).unwrap().tree_id();
        let side = commit_as(&fixture.repository, None, "Side", tree, [base], 100);
        fixture
            .repository
            .reference("refs/heads/side", side, false, "test")
            .unwrap();
        fixture
            .repository
            .reference("refs/heads/base", base, false, "test")
            .unwrap();
        let commits = select(&fixture, &["base..HEAD", "base..side"]).unwrap();
        let mut expected = fixture.commits[1..].to_vec();
        expected.push(side);
        assert_selection(commits, &expected);
    }

    #[test]
    fn rejects_revisions_beyond_x_and_ranges() {
        // "A...B" needs common ancestors, which a shallow clone may not
        // have, so it is rejected in every clone. libgit2 does not parse
        // "^X", "X^!" or "X^@" at all.
        let fixture = History::new();
        let result = select(&fixture, &["HEAD~2...HEAD"]);
        assert!(
            matches!(result, Err(Error::UnsupportedRevision)),
            "{result:?}"
        );
        for revision in ["^HEAD", "HEAD^!", "HEAD^@"] {
            let result = select(&fixture, &[revision]);
            assert!(
                matches!(result, Err(Error::ParseRevision(_))),
                "{revision}: {result:?}"
            );
        }
    }
}
