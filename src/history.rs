use gix::revision::plumbing::Spec;
use gix::{ObjectId, Repository};
use std::collections::{HashMap, HashSet};

use crate::{CommitId, Context, Error};

// Find the commits to validate: reachable from an included revision but
// not from an excluded one, with parents before their children.
pub(crate) fn resolve_commits(
    repo: &Repository,
    revisions: &[&str],
    all: bool,
) -> Result<Vec<CommitId>, Error> {
    if !all
        && let [input] = revisions
        && let Some(revision) = input.strip_suffix("^!")
    {
        // A single-commit selection does not need any parent history.
        let id = repo
            .rev_parse_single(revision)
            .context(Error::ParseRevision)?;
        return Ok(vec![CommitId(commit(repo, id.detach())?)]);
    }
    let revisions = parse_revisions(repo, revisions, all)?;
    let history = History::walk(repo, &revisions.include, &revisions.exclude)?;
    Ok(history.parents_first().into_iter().map(CommitId).collect())
}

// The commits named by the arguments, split into included and excluded.
struct Revisions {
    include: Vec<ObjectId>,
    exclude: Vec<ObjectId>,
}

// gix parses every revision form `git rev-list` accepts. Turn each into
// commits to include and exclude.
fn parse_revisions(repo: &Repository, revisions: &[&str], all: bool) -> Result<Revisions, Error> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    if all {
        let references = repo.references().context(Error::WalkHistory)?;
        for reference in references.all().context(Error::WalkHistory)? {
            let mut reference = reference.context(Error::WalkHistory)?;
            // As with `git rev-list --all`, refs to non-commit objects are ignored.
            if let Ok(commit) = reference.peel_to_commit() {
                include.push(commit.id);
            }
        }
        // An empty repository has no HEAD commit to include.
        if !repo.head().context(Error::ReadHead)?.is_unborn() {
            include.push(repo.head_commit().context(Error::ReadHead)?.id);
        }
    }
    for input in revisions {
        let spec = repo.rev_parse(*input).context(Error::ParseRevision)?;
        match spec.detach() {
            Spec::Include(id) => include.push(commit(repo, id)?),
            Spec::Exclude(id) => exclude.push(commit(repo, id)?),
            Spec::Range { from, to } => {
                exclude.push(commit(repo, from)?);
                include.push(commit(repo, to)?);
            }
            Spec::Merge { theirs, ours } => {
                let (theirs, ours) = (commit(repo, theirs)?, commit(repo, ours)?);
                include.extend([theirs, ours]);
                // Unrelated histories have no common ancestors to exclude.
                let bases = repo
                    .merge_bases_many(theirs, &[ours])
                    .context(Error::MergeBases)?;
                exclude.extend(bases.into_iter().map(|id| id.detach()));
            }
            Spec::IncludeOnlyParents(id) => include.extend(parents(repo, commit(repo, id)?)?),
            Spec::ExcludeParents(id) => {
                let id = commit(repo, id)?;
                include.push(id);
                exclude.extend(parents(repo, id)?);
            }
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

fn parents(repo: &Repository, id: ObjectId) -> Result<Vec<ObjectId>, Error> {
    let commit = repo.find_commit(id).context(Error::FindCommit)?;
    Ok(commit.parent_ids().map(|id| id.detach()).collect())
}

#[cfg(test)]
mod tests {
    use super::resolve_commits;
    use crate::Error;
    use crate::test_support::{commit_as, with_test_identity};
    use gix::refs::Target;
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
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

    fn select(fixture: &History, revisions: &[&str], all: bool) -> Result<Vec<ObjectId>, Error> {
        resolve_commits(&fixture.repository, revisions, all)
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

        let commits = select(&fixture, &[&format!("{base}..{head}")], false).unwrap();
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
            // "X^@" means "X's parents and their history", without X itself.
            (vec!["HEAD^@", "^base"], fixture.commits[1..3].to_vec()),
            // "X^!" means "only commit X".
            (vec!["HEAD^!", "side^!"], vec![fixture.commits[3], side]),
        ];
        for (args, expected) in cases {
            let commits = select(&fixture, &args, false).unwrap();
            assert_selection(commits, &expected);
        }
    }

    #[test]
    fn all_includes_branches_tags_and_detached_head() {
        // --all must select every commit reachable from a branch, a tag, or
        // HEAD. HEAD is detached here, so no branch points to "Detached";
        // it can only be found through HEAD.
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
        let tagged = commit_as(&fixture.repository, None, "Tagged", tree, [base], 200);
        fixture
            .repository
            .tag(
                "release",
                tagged,
                gix::objs::Kind::Commit,
                None,
                "Release",
                PreviousValue::MustNotExist,
            )
            .unwrap();
        let detached = commit_as(&fixture.repository, None, "Detached", tree, [base], 300);
        fixture
            .repository
            .edit_reference(RefEdit {
                change: Change::Update {
                    log: LogChange::default(),
                    expected: PreviousValue::Any,
                    new: Target::Object(detached),
                },
                name: "HEAD".try_into().unwrap(),
                deref: false,
            })
            .unwrap();
        let mut expected = fixture.commits.clone();
        expected.extend([side, tagged, detached]);
        let commits = select(&fixture, &[], true).unwrap();
        assert_selection(commits, &expected);
    }

    #[test]
    fn symmetric_ranges_hide_all_merge_bases_and_allow_unrelated_histories() {
        // "left...right" means "commits on either side, but not on both".
        // Left and Right are both merges of A and B, so they share two
        // ancestors. Both must be left out, leaving only Left and Right.
        let fixture = History::new();
        let base = fixture.commits[0];
        let tree = fixture
            .repository
            .find_commit(base)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let a = commit_as(&fixture.repository, None, "A", tree, [base], 100);
        let b = commit_as(&fixture.repository, None, "B", tree, [base], 900);
        let left = commit_as(&fixture.repository, None, "Left", tree, [a, b], 10);
        let right = commit_as(&fixture.repository, None, "Right", tree, [b, a], 20);
        // Make sure the setup really has two shared ancestors.
        assert_eq!(
            fixture
                .repository
                .merge_bases_many(left, &[right])
                .unwrap()
                .len(),
            2
        );
        let commits = select(&fixture, &[&format!("{left}...{right}")], false).unwrap();
        assert_selection(commits, &[left, right]);

        // Unrelated shares no history with HEAD. Instead of failing, the
        // range should select every commit from both sides.
        let unrelated = commit_as(&fixture.repository, None, "Unrelated", tree, [], 5);
        let commits = select(&fixture, &[&format!("HEAD...{unrelated}")], false).unwrap();
        let mut expected = fixture.commits.clone();
        expected.push(unrelated);
        assert_selection(commits, &expected);
    }
}
