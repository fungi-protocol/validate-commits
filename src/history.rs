use gix::revision::plumbing::Spec;
use gix::{ObjectId, Repository};
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::shallow::{cutoffs, fetch, missing_commit};
use crate::{CommitId, Context, Error};

// Find the commits to validate: reachable from an included revision but
// not from an excluded one, with parents before their children. In a
// shallow clone, fetch more history until the answer cannot change.
pub(crate) fn resolve_commits(
    repo: &Repository,
    revisions: &[&str],
    output: &mut impl Write,
) -> Result<Vec<CommitId>, Error> {
    let ids = if repo.is_shallow().context(Error::ReadShallow)? {
        resolve_shallow(repo, revisions, output)?
    } else {
        let revisions = parse_revisions(repo, revisions)?;
        History::walk(repo, &revisions.include, &revisions.exclude)?.parents_first(|_| true)
    };
    Ok(ids.into_iter().map(CommitId).collect())
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

// Fetch missing history until the selection cannot change, then select.
fn resolve_shallow(
    repo: &Repository,
    revisions: &[&str],
    output: &mut impl Write,
) -> Result<Vec<ObjectId>, Error> {
    let mut deepen = 8u32;
    loop {
        // A revision given as a full commit ID may not be in the clone yet,
        // like the base commit CI passes in. Fetch just those commits first.
        let mut missing: Vec<_> = revisions
            .iter()
            .flat_map(|input| input.strip_prefix('^').unwrap_or(input).split(".."))
            .filter_map(|name| missing_commit(repo, name))
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            fetch(repo, &missing, None, output)?;
            continue;
        }
        let parsed = parse_revisions(repo, revisions)?;
        let cutoffs = cutoffs(repo)?;
        // Each round walks all downloaded history behind the revisions.
        // That is cheap for CI-sized fetches but grows with deepening.
        let included = History::walk(repo, &parsed.include, &[])?;
        let excluded = History::walk(repo, &parsed.exclude, &[])?;
        let selected = included.parents_first(|id| !excluded.contains(id));
        if complete(&included, &excluded, &selected, &cutoffs) {
            return Ok(selected);
        }
        // Deepen from the cut-off points behind any revision. Ask for the
        // cut-off points, not the revisions: a local commit may not exist on
        // origin.
        let mut behind: Vec<_> = cutoffs
            .into_iter()
            .filter(|id| included.contains(id) || excluded.contains(id))
            .collect();
        behind.sort();
        fetch(repo, &behind, Some(deepen), output)?;
        deepen = deepen.saturating_mul(2);
    }
}

// Check whether missing history could still change the selected commits.
fn complete(
    included: &History,
    excluded: &History,
    selected: &[ObjectId],
    cutoffs: &HashSet<ObjectId>,
) -> bool {
    if cutoffs.is_empty() || selected.is_empty() {
        return true;
    }
    // A selected commit's parents are missing, so older selected commits
    // may be missing too.
    if selected.iter().any(|id| cutoffs.contains(id)) {
        return false;
    }
    let excluded_cutoffs: Vec<_> = cutoffs.iter().filter(|id| excluded.contains(id)).collect();
    // Missing excluded history could still exclude an apparent first
    // selected commit. It is safe only when that commit already descends
    // from every excluded cut-off point.
    selected.iter().all(|id| {
        let first = included.parents(id).all(|parent| excluded.contains(parent));
        !first
            || excluded_cutoffs
                .iter()
                .all(|cutoff| included.has_ancestor(*id, **cutoff))
    })
}

// Commits reachable from some starting points, and each commit's parents.
// gix stops at shallow boundaries, but still reports the real parents of
// a commit there.
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

    fn contains(&self, id: &ObjectId) -> bool {
        self.parents.contains_key(id)
    }

    fn parents(&self, id: &ObjectId) -> impl Iterator<Item = &ObjectId> {
        self.parents.get(id).into_iter().flatten()
    }

    // Whether `ancestor` can be reached from `id` through the walked history.
    fn has_ancestor(&self, id: ObjectId, ancestor: ObjectId) -> bool {
        let mut stack = vec![id];
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if id == ancestor {
                return true;
            }
            if seen.insert(id) {
                stack.extend(self.parents(&id).copied());
            }
        }
        false
    }

    // The commits `keep` accepts, with every parent before its children.
    // gix's topological walk can't be used: it loads every parent, which
    // fails at shallow boundaries.
    fn parents_first(&self, keep: impl Fn(&ObjectId) -> bool) -> Vec<ObjectId> {
        let mut ids: Vec<_> = self.parents.keys().filter(|id| keep(id)).copied().collect();
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
    use crate::test_support::{Remote, commit_as, git, with_test_identity};
    use gix::refs::transaction::PreviousValue;
    use gix::{ObjectId, Repository};
    use std::fs;
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
        resolve_commits(repository, revisions, &mut Vec::new())
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

    // Shallow clone tests: history is built in a "remote" repository, then
    // cloned with only a few commits, the way CI checks out a pull request.
    // The tool must fetch just enough missing history to pick the right
    // commits.
    #[test]
    fn shallow_clones_fetch_enough_history_to_select_a_range() {
        // A pull request of three commits merged on top of a long main
        // line. At depth 1 the base commit is missing entirely; at depth 2
        // it is present but the PR's history is cut off. Either way, the
        // tool must fetch more history and then select exactly the PR.
        for depth in [1, 2] {
            let remote = Remote::new();
            let base = *remote.chain(None, 20, "Main").last().unwrap();
            let mut selected = remote.chain(Some(base), 3, "PR");
            let head = remote.merge([base, *selected.last().unwrap()]);
            selected.push(head);
            let clone = remote.clone_shallow(head, depth);
            fs::write(clone.path().join("FETCH_HEAD"), "keep me\n").unwrap();

            let commits = select_in(&clone, &[&format!("{base}..HEAD")]).unwrap();
            assert_selection(commits, &selected);
            // Fetching must not overwrite FETCH_HEAD, and should not
            // download the whole history.
            assert_eq!(
                fs::read_to_string(clone.path().join("FETCH_HEAD")).unwrap(),
                "keep me\n"
            );
            assert!(clone.is_shallow().unwrap());
        }
    }

    #[test]
    fn shallow_clones_fetch_base_history_to_exclude_an_old_fork_point() {
        // The PR branched from Fork, which is old history of the base
        // (ten commits back). Before enough base history is fetched, Fork
        // looks like part of the PR. The tool must keep fetching until it
        // can see that Fork is already in the base, and select only PR and
        // the merge.
        let remote = Remote::new();
        let shared = *remote.chain(None, 3, "Shared").last().unwrap();
        let fork = *remote.chain(Some(shared), 1, "Fork").last().unwrap();
        let branch = *remote.chain(Some(shared), 1, "Branch").last().unwrap();
        let main = *remote.chain(Some(fork), 10, "Main").last().unwrap();
        let base = remote.merge([main, branch]);
        let pr = *remote.chain(Some(fork), 1, "PR").last().unwrap();
        let head = remote.merge([base, pr]);
        let clone = remote.clone_shallow(head, 1);

        let commits = select_in(&clone, &[&format!("{base}..HEAD")]).unwrap();
        assert_selection(commits, &[pr, head]);
    }

    #[test]
    fn shallow_clones_fetch_history_for_local_commits_origin_has_not_seen() {
        // A developer with a shallow clone adds a commit that is not pushed
        // yet. Origin cannot send history for that commit by its ID, so the
        // tool must ask for history of the cut-off commits origin does know.
        let remote = Remote::new();
        let base = *remote.chain(None, 20, "Main").last().unwrap();
        let mut selected = remote.chain(Some(base), 3, "PR");
        let clone = remote.clone_shallow(*selected.last().unwrap(), 2);
        let head = clone.head_commit().unwrap();
        let tree = head.tree_id().unwrap().detach();
        let local = commit_as(&clone, Some("HEAD"), "Local", tree, [head.id], 0);
        selected.push(local);

        let commits = select_in(&clone, &[&format!("{base}..HEAD")]).unwrap();
        assert_selection(commits, &selected);
    }

    #[test]
    fn shallow_clones_accept_included_and_excluded_revisions() {
        // Besides "A..B", a shallow clone accepts plain and "^" revisions,
        // in any combination, because they only include or exclude history.
        // A side branch from the base is selected along with the PR.
        let remote = Remote::new();
        let base = *remote.chain(None, 20, "Main").last().unwrap();
        let mut selected = remote.chain(Some(base), 3, "PR");
        let side = remote.chain(Some(base), 2, "Side");
        let head = remote.merge([*selected.last().unwrap(), *side.last().unwrap()]);
        selected.extend(side);
        selected.push(head);
        let clone = remote.clone_shallow(head, 1);

        let excluded = format!("^{base}");
        let commits = select_in(&clone, &["HEAD", &excluded]).unwrap();
        assert_selection(commits, &selected);
    }

    #[test]
    fn shallow_clones_report_fetch_failures() {
        // If history cannot be fetched, the tool must stop with an error
        // instead of checking a wrong set of commits.
        let remote = Remote::new();
        let base = *remote.chain(None, 20, "Main").last().unwrap();
        let head = *remote.chain(Some(base), 3, "PR").last().unwrap();
        let clone = remote.clone_shallow(head, 1);
        git(
            clone.workdir().unwrap(),
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/validate-commits",
            ],
        );

        let result = select_in(&clone, &[&format!("{base}..HEAD")]);
        assert!(
            matches!(result, Err(Error::FetchFailed { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn shallow_clones_stop_when_origin_is_also_shallow() {
        // Origin is itself shallow: it has the newest commit and the base,
        // but not the history between them. The tool must notice that
        // fetching made no progress and stop with a clear error.
        let remote = Remote::new();
        let commits = remote.chain(None, 20, "Main");
        let (base, head) = (commits[10], *commits.last().unwrap());
        let origin = remote.clone_shallow(head, 1);
        let origin_dir = origin.workdir().unwrap().to_owned();
        git(
            &origin_dir,
            &["fetch", "--quiet", "--depth=1", "origin", &base.to_string()],
        );
        git(
            &origin_dir,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let root = tempfile::tempdir().unwrap();
        let url = format!("file://{}", origin_dir.display());
        git(
            root.path(),
            &["clone", "--quiet", "--depth=1", &url, "clone"],
        );
        let clone = gix::open(root.path().join("clone")).unwrap();

        let result = select_in(&clone, &[&format!("{base}..HEAD")]);
        assert!(
            matches!(result, Err(Error::NoHistoryProgress)),
            "{result:?}"
        );
    }
}
