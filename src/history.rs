use gix::revision::plumbing::Spec;
use gix::{ObjectId, Repository};
use std::collections::{HashMap, HashSet};

use crate::{CommitId, Context, Error};

// Find the commits to validate: reachable from an included revision but
// not from an excluded one, with parents before their children.
pub(crate) fn resolve_commits(
    repo: &Repository,
    revisions: &[&str],
) -> Result<Vec<CommitId>, Error> {
    let revisions = parse_revisions(repo, revisions)?;
    let history = History::walk(repo, &revisions.include, &revisions.exclude)?;
    Ok(history.parents_first().into_iter().map(CommitId).collect())
}

// The commits named by the arguments, split into included and excluded.
struct Revisions {
    include: Vec<ObjectId>,
    exclude: Vec<ObjectId>,
}

// Accept only X, ^X and A..B. Other forms (X^!, X^@, A...B) need parents
// or common ancestors, which a shallow clone may not have downloaded.
// Rejecting them everywhere keeps full and shallow clones consistent.
fn parse_revisions(repo: &Repository, revisions: &[&str]) -> Result<Revisions, Error> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for input in revisions {
        let spec = repo.rev_parse(*input).context(Error::ParseRevision)?;
        match spec.detach() {
            Spec::Include(id) => include.push(commit(repo, id)?),
            Spec::Exclude(id) => exclude.push(commit(repo, id)?),
            Spec::Range { from, to } => {
                exclude.push(commit(repo, from)?);
                include.push(commit(repo, to)?);
            }
            _ => return Err(Error::UnsupportedRevision),
        }
    }
    Ok(Revisions { include, exclude })
}

// Commits reachable from some starting points, and each commit's parents.
struct History {
    parents: HashMap<ObjectId, Vec<ObjectId>>,
}

impl History {
    fn walk(repo: &Repository, from: &[ObjectId], hide: &[ObjectId]) -> Result<Self, Error> {
        let mut parents = HashMap::new();
        let walk = repo
            .rev_walk(from.iter().copied())
            .with_hidden(hide.iter().copied())
            .all()
            .context(Error::WalkHistory)?;
        for info in walk {
            let info = info.context(Error::WalkHistory)?;
            parents.insert(info.id, info.parent_ids.to_vec());
        }
        Ok(Self { parents })
    }

    fn parents(&self, id: &ObjectId) -> impl Iterator<Item = &ObjectId> {
        self.parents.get(id).into_iter().flatten()
    }

    // The walked commits, with every parent before its children. gix's
    // topological walk can't be used: it loads every parent, which fails at
    // shallow boundaries.
    fn parents_first(&self) -> Vec<ObjectId> {
        let mut ids: Vec<_> = self.parents.keys().copied().collect();
        ids.sort();
        let kept: HashSet<_> = ids.iter().copied().collect();
        let mut order = Vec::with_capacity(ids.len());
        let mut done = HashSet::new();
        for id in ids {
            let mut stack = vec![(id, false)];
            while let Some((id, expanded)) = stack.pop() {
                if expanded {
                    if done.insert(id) {
                        order.push(id);
                    }
                    continue;
                }
                if done.contains(&id) {
                    continue;
                }
                stack.push((id, true));
                for parent in self.parents(&id) {
                    if kept.contains(parent) && !done.contains(parent) {
                        stack.push((*parent, false));
                    }
                }
            }
        }
        order
    }
}

// Peel an object to its commit, following tags if necessary.
fn commit(repo: &Repository, id: ObjectId) -> Result<ObjectId, Error> {
    let object = repo.find_object(id).context(Error::PeelToCommit)?;
    Ok(object.peel_to_commit().context(Error::PeelToCommit)?.id)
}

#[cfg(test)]
mod tests {
    use super::resolve_commits;
    use crate::Error;
    use crate::test_support::{commit_as, with_test_identity};
    use gix::refs::transaction::PreviousValue;
    use gix::{ObjectId, Repository};
    use tempfile::TempDir;

    struct History {
        _root: TempDir,
        repository: Repository,
        commits: Vec<ObjectId>,
    }

    impl History {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let repository = with_test_identity(gix::init(root.path()).unwrap());
            let tree = repository.empty_tree().id;
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

    fn select(fixture: &History, revisions: &[&str]) -> Result<Vec<ObjectId>, Error> {
        select_in(&fixture.repository, revisions)
    }

    fn select_in(repository: &Repository, revisions: &[&str]) -> Result<Vec<ObjectId>, Error> {
        resolve_commits(repository, revisions)
            .map(|commits| commits.into_iter().map(|commit| commit.0).collect())
    }

    fn assert_selection(mut actual: Vec<ObjectId>, expected: &[ObjectId]) {
        actual.sort();
        let mut expected = expected.to_vec();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn orders_merge_parents_with_skewed_dates() {
        let fixture = History::new();
        let base = fixture.commits[3];
        let tree = fixture
            .repository
            .find_commit(base)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let commit = |message, parents: &[ObjectId], timestamp| {
            commit_as(
                &fixture.repository,
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
    fn combines_multiple_revisions_ranges_and_exclusions() {
        // History: Base -> First -> Second -> Third (HEAD), plus a "side"
        // branch that also starts from Base. Each case passes several
        // arguments at once, like `git log`, and checks what they select
        // together.
        let fixture = History::new();
        let base = fixture.commits[0];
        let tree = fixture
            .repository
            .find_commit(base)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let side = commit_as(&fixture.repository, None, "Side", tree, [base], 100);
        fixture
            .repository
            .reference("refs/heads/side", side, PreviousValue::Any, "test")
            .unwrap();
        fixture
            .repository
            .reference("refs/heads/base", base, PreviousValue::Any, "test")
            .unwrap();
        let cases = [
            // "^base" leaves out Base and everything before it.
            (
                vec!["HEAD", "side", "^base"],
                vec![
                    fixture.commits[1],
                    fixture.commits[2],
                    fixture.commits[3],
                    side,
                ],
            ),
            // "base..X" means "commits in X that are not in base".
            (
                vec!["base..HEAD", "base..side"],
                vec![
                    fixture.commits[1],
                    fixture.commits[2],
                    fixture.commits[3],
                    side,
                ],
            ),
        ];
        for (args, expected) in cases {
            let commits = select(&fixture, &args).unwrap();
            assert_selection(commits, &expected);
        }
    }

    #[test]
    fn rejects_revisions_that_need_parents_or_common_ancestors() {
        // "X^!", "X^@" and "A...B" depend on parents or common ancestors,
        // which a shallow clone may not have. They are rejected in every
        // clone so a command behaves the same in CI and locally.
        let fixture = History::new();
        for revision in ["HEAD^!", "HEAD^@", "HEAD~2...HEAD"] {
            let result = select(&fixture, &[revision]);
            assert!(
                matches!(result, Err(Error::UnsupportedRevision)),
                "{revision}: {result:?}"
            );
        }
    }
}
