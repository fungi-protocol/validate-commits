use git2::{ErrorCode, Object, Oid, Repository, RevparseMode, Revwalk, Sort};

use crate::{CommitId, Error};

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
        return Ok(vec![CommitId(resolve(repo, revision)?.id())]);
    }

    let mut walk = repo.revwalk().map_err(Error::WalkHistory)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
        .map_err(Error::WalkHistory)?;

    if all {
        walk.push_glob("refs/*").map_err(Error::WalkHistory)?;
        match repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit().map_err(Error::PeelToCommit)?;
                walk.push(commit.id()).map_err(Error::WalkHistory)?;
            }
            // An empty repository has no HEAD commit to include.
            Err(error) if error.code() == ErrorCode::UnbornBranch => {}
            Err(error) => return Err(Error::ParseRevision(error)),
        }
    }
    for input in revisions {
        add_revision(repo, &mut walk, input)?;
    }

    walk.map(|id| id.map(CommitId).map_err(Error::WalkHistory))
        .collect()
}

fn add_revision(repo: &Repository, walk: &mut Revwalk<'_>, input: &str) -> Result<(), Error> {
    // libgit2 parses single revisions and ranges. Expand the remaining
    // revision-set shorthands into its native push/hide operations.
    if let Some(revision) = input.strip_prefix('^') {
        walk.hide(resolve(repo, revision)?.id())
            .map_err(Error::WalkHistory)?;
    } else if let Some(revision) = input.strip_suffix("^!") {
        let commit = resolve(repo, revision)?;
        walk.push(commit.id()).map_err(Error::WalkHistory)?;
        for parent in commit.parent_ids() {
            walk.hide(parent).map_err(Error::WalkHistory)?;
        }
    } else if let Some(revision) = input.strip_suffix("^@") {
        for parent in resolve(repo, revision)?.parent_ids() {
            walk.push(parent).map_err(Error::WalkHistory)?;
        }
    } else {
        let spec = repo.revparse(input).map_err(Error::ParseRevision)?;
        let from = peel(spec.from().expect("libgit2 supplies the range start"))?;
        match spec.mode() {
            mode if mode.contains(RevparseMode::SINGLE) => {
                walk.push(from).map_err(Error::WalkHistory)?;
            }
            mode if mode.contains(RevparseMode::MERGE_BASE) => {
                let to = peel(spec.to().expect("libgit2 supplies the range end"))?;
                walk.push(from).map_err(Error::WalkHistory)?;
                walk.push(to).map_err(Error::WalkHistory)?;
                match repo.merge_bases(from, to) {
                    Ok(bases) => {
                        for base in bases.iter() {
                            walk.hide(*base).map_err(Error::WalkHistory)?;
                        }
                    }
                    // Unrelated histories have no common ancestors to hide.
                    Err(error) if error.code() == ErrorCode::NotFound => {}
                    Err(error) => return Err(Error::MergeBases(error)),
                }
            }
            mode if mode.contains(RevparseMode::RANGE) => {
                let to = peel(spec.to().expect("libgit2 supplies the range end"))?;
                walk.hide(from).map_err(Error::WalkHistory)?;
                walk.push(to).map_err(Error::WalkHistory)?;
            }
            mode => return Err(Error::UnsupportedRevisionMode(mode)),
        }
    }
    Ok(())
}

fn resolve<'repo>(repo: &'repo Repository, revision: &str) -> Result<git2::Commit<'repo>, Error> {
    repo.revparse_single(revision)
        .map_err(Error::ParseRevision)?
        .peel_to_commit()
        .map_err(Error::PeelToCommit)
}

fn peel(object: &Object<'_>) -> Result<Oid, Error> {
    object
        .peel_to_commit()
        .map(|commit| commit.id())
        .map_err(Error::PeelToCommit)
}

#[cfg(test)]
mod tests {
    use super::{Oid, Repository, resolve_commits};
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

    fn select(fixture: &History, revisions: &[&str], all: bool) -> Result<Vec<Oid>, crate::Error> {
        resolve_commits(&fixture.repository, revisions, all)
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
        let commit = |message, parents: &[Oid], timestamp| {
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
        let tree = fixture.repository.find_commit(base).unwrap().tree_id();
        let side = commit_as(&fixture.repository, None, "Side", tree, [base], 100);
        fixture
            .repository
            .reference("refs/heads/side", side, false, "test")
            .unwrap();
        let tagged = commit_as(&fixture.repository, None, "Tagged", tree, [base], 200);
        fixture
            .repository
            .tag(
                "release",
                &fixture.repository.find_object(tagged, None).unwrap(),
                &signature(200),
                "Release",
                false,
            )
            .unwrap();
        let detached = commit_as(&fixture.repository, None, "Detached", tree, [base], 300);
        fixture.repository.set_head_detached(detached).unwrap();
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
        let tree = fixture.repository.find_commit(base).unwrap().tree_id();
        let a = commit_as(&fixture.repository, None, "A", tree, [base], 100);
        let b = commit_as(&fixture.repository, None, "B", tree, [base], 900);
        let left = commit_as(&fixture.repository, None, "Left", tree, [a, b], 10);
        let right = commit_as(&fixture.repository, None, "Right", tree, [b, a], 20);
        // Make sure the setup really has two shared ancestors.
        assert_eq!(
            fixture.repository.merge_bases(left, right).unwrap().len(),
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
