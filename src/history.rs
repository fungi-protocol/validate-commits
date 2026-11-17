use git2::{Oid, Repository, RevparseMode, Sort};
use std::collections::HashSet;
use std::io::Write;

use crate::shallow::{cutoffs, fetch, missing_commit};
use crate::{CommitId, Error};

// Find the commits to validate: reachable from an included revision but
// not from an excluded one, with parents before their children. In a
// shallow clone, fetch more history until the answer cannot change.
pub(crate) fn resolve_commits(
    repo: &mut Repository,
    revisions: &[&str],
    output: &mut impl Write,
) -> Result<Vec<CommitId>, Error> {
    let mut deepen = 8u32;
    loop {
        // A revision given as a full commit ID may not be in the clone yet,
        // like the base commit CI passes in. Fetch just those commits first.
        let mut missing: Vec<_> = revisions
            .iter()
            .flat_map(|input| input.split(".."))
            .filter_map(|name| missing_commit(repo, name))
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            fetch(repo, &missing, None, output)?;
            continue;
        }
        let parsed = parse_revisions(repo, revisions)?;
        let selected = walk(repo, &parsed.include, &parsed.exclude)?;
        if !repo.is_shallow() {
            return Ok(selected.into_iter().map(CommitId).collect());
        }
        // Only cut-off points behind a revision can change the selection.
        let behind: HashSet<_> = cutoffs(repo)?
            .into_iter()
            .filter(|cutoff| {
                parsed
                    .include
                    .iter()
                    .chain(&parsed.exclude)
                    .any(|id| descends(repo, *id, *cutoff))
            })
            .collect();
        if complete(repo, &parsed, &selected, &behind) {
            return Ok(selected.into_iter().map(CommitId).collect());
        }
        // Deepen from the cut-off points, not the revisions: a local commit
        // may not exist on origin.
        let mut behind: Vec<_> = behind.into_iter().collect();
        behind.sort();
        fetch(repo, &behind, Some(deepen), output)?;
        deepen = deepen.saturating_mul(2);
    }
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

// The selected commits, parents first. libgit2's walk treats the commits
// where a shallow clone stops as roots.
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

// Whether `ancestor` is `id` or reachable from it in the downloaded history.
fn descends(repo: &Repository, id: Oid, ancestor: Oid) -> bool {
    id == ancestor || repo.graph_descendant_of(id, ancestor).unwrap_or(false)
}

// Check whether missing history could still change the selected commits.
fn complete(
    repo: &Repository,
    revisions: &Revisions,
    selected: &[Oid],
    cutoffs: &HashSet<Oid>,
) -> bool {
    if cutoffs.is_empty() || selected.is_empty() {
        return true;
    }
    // A selected commit's parents are missing, so older selected commits
    // may be missing too.
    if selected.iter().any(|id| cutoffs.contains(id)) {
        return false;
    }
    let excluded_cutoffs: Vec<_> = cutoffs
        .iter()
        .filter(|cutoff| {
            revisions
                .exclude
                .iter()
                .any(|id| descends(repo, *id, **cutoff))
        })
        .collect();
    // Missing excluded history could still exclude an apparent first
    // selected commit, one whose parents are all excluded. It is safe only
    // when that commit already descends from every excluded cut-off point.
    let chosen: HashSet<_> = selected.iter().collect();
    selected.iter().all(|id| {
        let first = repo
            .find_commit(*id)
            .map(|commit| commit.parent_ids().all(|parent| !chosen.contains(&parent)))
            .unwrap_or(true);
        !first
            || excluded_cutoffs
                .iter()
                .all(|cutoff| descends(repo, *id, **cutoff))
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_commits;
    use crate::Error;
    use crate::test_support::{Remote, commit_as, git};
    use git2::{Oid, Repository};
    use std::fs;
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

    fn select(fixture: &mut History, revisions: &[&str]) -> Result<Vec<Oid>, Error> {
        select_in(&mut fixture.repository, revisions)
    }

    fn select_in(repository: &mut Repository, revisions: &[&str]) -> Result<Vec<Oid>, Error> {
        resolve_commits(repository, revisions, &mut Vec::new())
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
        let mut fixture = History::new();
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

        let commits = select(&mut fixture, &[&format!("{base}..{head}")]).unwrap();
        assert_eq!(commits, vec![fork, left, a, b, head]);
    }

    #[test]
    fn combines_multiple_revisions_and_ranges() {
        // History: Base -> First -> Second -> Third (HEAD), plus a "side"
        // branch that also starts from Base. Several arguments select the
        // union of what each one selects, like `git log`.
        let mut fixture = History::new();
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
        let commits = select(&mut fixture, &["base..HEAD", "base..side"]).unwrap();
        let mut expected = fixture.commits[1..].to_vec();
        expected.push(side);
        assert_selection(commits, &expected);
    }

    #[test]
    fn rejects_revisions_beyond_x_and_ranges() {
        // "A...B" needs common ancestors, which a shallow clone may not
        // have, so it is rejected in every clone. libgit2 does not parse
        // "^X", "X^!" or "X^@" at all.
        let mut fixture = History::new();
        let result = select(&mut fixture, &["HEAD~2...HEAD"]);
        assert!(
            matches!(result, Err(Error::UnsupportedRevision)),
            "{result:?}"
        );
        for revision in ["^HEAD", "HEAD^!", "HEAD^@"] {
            let result = select(&mut fixture, &[revision]);
            assert!(
                matches!(result, Err(Error::ParseRevision(_))),
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
            let mut clone = remote.clone_shallow(head, depth);
            fs::write(clone.path().join("FETCH_HEAD"), "keep me\n").unwrap();

            let commits = select_in(&mut clone, &[&format!("{base}..HEAD")]).unwrap();
            assert_selection(commits, &selected);
            // Fetching must not overwrite FETCH_HEAD, and should not
            // download the whole history.
            assert_eq!(
                fs::read_to_string(clone.path().join("FETCH_HEAD")).unwrap(),
                "keep me\n"
            );
            assert!(clone.is_shallow());
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
        let mut clone = remote.clone_shallow(head, 1);

        let commits = select_in(&mut clone, &[&format!("{base}..HEAD")]).unwrap();
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
        let mut clone = remote.clone_shallow(*selected.last().unwrap(), 2);
        let head = clone.head().unwrap().peel_to_commit().unwrap();
        let local = commit_as(
            &clone,
            Some("HEAD"),
            "Local",
            head.tree_id(),
            [head.id()],
            0,
        );
        drop(head);
        selected.push(local);

        let commits = select_in(&mut clone, &[&format!("{base}..HEAD")]).unwrap();
        assert_selection(commits, &selected);
    }

    #[test]
    fn shallow_clones_fetch_every_missing_range_end() {
        // Two ranges whose ends are both cut off from a depth-1 clone. Each
        // end is fetched by its ID, and together they select both branches.
        let remote = Remote::new();
        let base = *remote.chain(None, 20, "Main").last().unwrap();
        let pr = remote.chain(Some(base), 3, "PR");
        let side = remote.chain(Some(base), 2, "Side");
        let (pr_tip, side_tip) = (*pr.last().unwrap(), *side.last().unwrap());
        let head = remote.merge([pr_tip, side_tip]);
        let mut clone = remote.clone_shallow(head, 1);

        let ranges = [format!("{base}..{pr_tip}"), format!("{base}..{side_tip}")];
        let commits = select_in(&mut clone, &[&ranges[0], &ranges[1]]).unwrap();
        assert_selection(commits, &[pr, side].concat());
    }

    #[test]
    fn shallow_clones_report_fetch_failures() {
        // If history cannot be fetched, the tool must stop with an error
        // instead of checking a wrong set of commits.
        let remote = Remote::new();
        let base = *remote.chain(None, 20, "Main").last().unwrap();
        let head = *remote.chain(Some(base), 3, "PR").last().unwrap();
        let mut clone = remote.clone_shallow(head, 1);
        git(
            clone.workdir().unwrap(),
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/validate-commits",
            ],
        );

        let result = select_in(&mut clone, &[&format!("{base}..HEAD")]);
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
        let mut clone = Repository::open(root.path().join("clone")).unwrap();

        let result = select_in(&mut clone, &[&format!("{base}..HEAD")]);
        assert!(
            matches!(result, Err(Error::NoHistoryProgress)),
            "{result:?}"
        );
    }
}
