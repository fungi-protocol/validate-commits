use git2::{ErrorCode, Object, Oid, Repository, RevparseMode, Sort};
use std::collections::HashSet;
use std::fs;
use std::process::{Command, Stdio};

use crate::{CommitId, Error};

// Find the commits to validate, fetching more history only when needed.
pub(crate) fn resolve_commits(
    repo: &Repository,
    revisions: &[&str],
    all: bool,
) -> Result<Vec<CommitId>, Error> {
    let single_revisions = if all {
        None
    } else {
        revisions
            .iter()
            .map(|input| input.strip_suffix("^!"))
            .collect::<Option<Vec<_>>>()
    };

    let mut deepen = 8u32;
    loop {
        // Reopen after fetching so cached commits use the new shallow boundaries.
        let repo = Repository::open(repo.path()).map_err(Error::DiscoverRepository)?;
        // Even after fetching a missing SHA, one commit needs no parent history.
        if let Some(revisions) = &single_revisions {
            let mut ids = revisions
                .iter()
                .map(|revision| resolve(&repo, revision).map(|commit| commit.id()));
            if let Some(Ok(id)) = ids.next()
                && ids.all(|other| other.is_ok_and(|other| other == id))
            {
                return Ok(vec![CommitId(id)]);
            }
        }
        // Linked worktrees share shallow history, but keep their own HEAD and refs.
        let common = common_repository(&repo)?;
        let history_repo = common.as_ref().unwrap_or(&repo);
        let shallow = boundaries(&repo)?;
        let mut truncated = HashSet::new();
        for id in &shallow {
            let commit = history_repo.find_commit(*id).map_err(Error::FindCommit)?;
            if has_parents(&commit)? {
                truncated.insert(*id);
            }
        }
        let selection = match select_revisions(&repo, history_repo, revisions, all, &truncated) {
            Ok(selection) => selection,
            Err(error @ Error::ParseRevision(_)) if !shallow.is_empty() => {
                let missing = missing_shas(&repo, revisions)?;
                if missing.is_empty() {
                    return Err(error);
                }
                fetch(&repo, &missing, None)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let selected = walk(history_repo, &selection.tips, &selection.hidden)?;
        if selection.needs_parents.is_empty()
            && complete(history_repo, &selection, &selected, &truncated)?
        {
            return Ok(selected.into_iter().map(CommitId).collect());
        }
        let mut tips = selection.tips;
        tips.extend(selection.hidden);
        tips.extend(selection.needs_parents);
        tips.sort();
        tips.dedup();
        // Fetch the missing ancestry at its cutoff, since local tips may not exist on origin.
        let mut incomplete = Vec::new();
        for boundary in &truncated {
            for id in &tips {
                if id == boundary
                    || history_repo
                        .graph_descendant_of(*id, *boundary)
                        .map_err(Error::WalkHistory)?
                {
                    incomplete.push(*boundary);
                    break;
                }
            }
        }
        incomplete.sort();
        fetch(&repo, &incomplete, Some(deepen))?;
        deepen = deepen.saturating_mul(2);
    }
}

#[derive(Default)]
struct Selection {
    tips: Vec<Oid>,
    hidden: Vec<Oid>,
    needs_parents: Vec<Oid>,
}

// Turn revision arguments into commits to include, exclude, or fetch parents for.
fn select_revisions(
    repo: &Repository,
    history_repo: &Repository,
    revisions: &[&str],
    all: bool,
    truncated: &HashSet<Oid>,
) -> Result<Selection, Error> {
    let mut selection = Selection::default();
    if all {
        for reference in repo.references().map_err(Error::WalkHistory)? {
            let reference = reference.map_err(Error::WalkHistory)?;
            // As with Revwalk::push_glob, refs to non-commit objects are ignored.
            if let Ok(commit) = reference.peel_to_commit() {
                selection.tips.push(commit.id());
            }
        }
        match repo.head() {
            Ok(head) => selection
                .tips
                .push(head.peel_to_commit().map_err(Error::PeelToCommit)?.id()),
            Err(error) if error.code() == ErrorCode::UnbornBranch => {}
            Err(error) => return Err(Error::ParseRevision(error)),
        }
    }
    for input in revisions {
        if let Some(revision) = input.strip_prefix('^') {
            selection.hidden.push(resolve(repo, revision)?.id());
        } else if let Some(revision) = input.strip_suffix("^!") {
            let commit = resolve(repo, revision)?;
            selection.tips.push(commit.id());
            if truncated.contains(&commit.id()) {
                selection.needs_parents.push(commit.id());
            } else {
                selection.hidden.extend(commit.parent_ids());
            }
        } else if let Some(revision) = input.strip_suffix("^@") {
            let commit = resolve(repo, revision)?;
            if truncated.contains(&commit.id()) {
                selection.needs_parents.push(commit.id());
            } else {
                selection.tips.extend(commit.parent_ids());
            }
        } else {
            let spec = repo.revparse(input).map_err(Error::ParseRevision)?;
            let from = peel(spec.from().expect("libgit2 supplies the range start"))?;
            match spec.mode() {
                mode if mode.contains(RevparseMode::SINGLE) => selection.tips.push(from),
                mode if mode.contains(RevparseMode::MERGE_BASE) => {
                    let to = peel(spec.to().expect("libgit2 supplies the range end"))?;
                    selection.tips.extend([from, to]);
                    match history_repo.merge_bases(from, to) {
                        Ok(bases) => selection.hidden.extend(bases.iter().copied()),
                        Err(error) if error.code() == ErrorCode::NotFound => {}
                        Err(error) => return Err(Error::MergeBases(error)),
                    }
                }
                mode if mode.contains(RevparseMode::RANGE) => {
                    let to = peel(spec.to().expect("libgit2 supplies the range end"))?;
                    selection.tips.push(to);
                    selection.hidden.push(from);
                }
                mode => return Err(Error::UnsupportedRevisionMode(mode)),
            }
        }
    }
    // Excluding a commit also excludes its ancestors and their missing parents.
    if !selection.needs_parents.is_empty() {
        let excluded: HashSet<_> = walk(history_repo, &selection.hidden, &[])?
            .into_iter()
            .collect();
        selection.needs_parents.retain(|id| !excluded.contains(id));
    }
    Ok(selection)
}

// Use git2 to list selected commits with parents before their children.
fn walk(repo: &Repository, tips: &[Oid], hidden: &[Oid]) -> Result<Vec<Oid>, Error> {
    let mut walk = repo.revwalk().map_err(Error::WalkHistory)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
        .map_err(Error::WalkHistory)?;
    for id in tips {
        walk.push(*id).map_err(Error::WalkHistory)?;
    }
    for id in hidden {
        walk.hide(*id).map_err(Error::WalkHistory)?;
    }
    walk.map(|id| id.map_err(Error::WalkHistory)).collect()
}

// Check whether missing history could still change the selected commits.
fn complete(
    repo: &Repository,
    selection: &Selection,
    selected: &[Oid],
    truncated: &HashSet<Oid>,
) -> Result<bool, Error> {
    if truncated.is_empty() || selected.is_empty() {
        return Ok(true);
    }
    if selected.iter().any(|id| truncated.contains(id)) {
        return Ok(false);
    }
    let excluded: HashSet<_> = walk(repo, &selection.hidden, &[])?.into_iter().collect();
    let hidden_boundaries: HashSet<_> = excluded.intersection(truncated).copied().collect();
    if hidden_boundaries.is_empty() {
        return Ok(true);
    }
    // Missing base history could still exclude an apparent first selected commit.
    // It is safe only when that commit already descends from every hidden boundary.
    for id in selected {
        let commit = repo.find_commit(*id).map_err(Error::FindCommit)?;
        if commit.parent_ids().all(|parent| excluded.contains(&parent)) {
            for boundary in &hidden_boundaries {
                if !repo
                    .graph_descendant_of(*id, *boundary)
                    .map_err(Error::WalkHistory)?
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

// Find full commit IDs in the arguments that are missing from this clone.
fn missing_shas(repo: &Repository, revisions: &[&str]) -> Result<Vec<Oid>, Error> {
    let odb = repo.odb().map_err(Error::FindCommit)?;
    let mut missing = Vec::new();
    for input in revisions {
        let input = input.strip_prefix('^').unwrap_or(input);
        let input = input
            .strip_suffix("^!")
            .or_else(|| input.strip_suffix("^@"))
            .unwrap_or(input);
        for endpoint in input.split("..").map(|part| part.trim_start_matches('.')) {
            // Do not fetch abbreviations or unresolved parent expressions such as SHA~1.
            if endpoint.len() == 40
                && let Ok(id) = Oid::from_str(endpoint)
                && !odb.exists(id)
            {
                missing.push(id);
            }
        }
    }
    missing.sort();
    missing.dedup();
    Ok(missing)
}

// Check whether a commit originally had parents, even if this clone hides them.
fn has_parents(commit: &git2::Commit<'_>) -> Result<bool, Error> {
    match commit.header_field_bytes("parent") {
        Ok(_) => Ok(true),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(false),
        Err(error) => Err(Error::FindCommit(error)),
    }
}

// Check whether any history is still cut off by a shallow clone.
pub(crate) fn is_shallow(repo: &Repository) -> Result<bool, Error> {
    Ok(!boundaries(repo)?.is_empty())
}

// Read the commits where this clone's downloaded history stops.
fn boundaries(repo: &Repository) -> Result<HashSet<Oid>, Error> {
    let text = match fs::read_to_string(repo.commondir().join("shallow")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(Error::ReadShallowCommits(error)),
    };
    text.lines()
        .map(|line| Oid::from_str(line).map_err(Error::ParseShallowCommit))
        .collect()
}

// Fetch commits or more ancestry without changing branches or staged files.
fn fetch(repo: &Repository, tips: &[Oid], deepen: Option<u32>) -> Result<(), Error> {
    let before = boundaries(repo)?;
    let depth_flag = match deepen {
        Some(count) => {
            eprintln!("Deepening history by {count} commits");
            format!("--deepen={count}")
        }
        None => "--depth=1".to_owned(),
    };
    // Git handles transport, authentication, and relative deepening.
    let status = Command::new("git")
        .current_dir(repo.workdir().ok_or(Error::BareRepository)?)
        .args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
        ])
        .arg(depth_flag)
        .arg("origin")
        .args(tips.iter().map(ToString::to_string))
        .stdout(Stdio::null())
        .status()
        .map_err(Error::SpawnGit)?;
    if !status.success() {
        return Err(Error::FetchFailed { status });
    }
    if deepen.is_some() && boundaries(repo)? == before {
        return Err(Error::NoHistoryProgress);
    }
    let odb = repo.odb().map_err(Error::FindCommit)?;
    odb.refresh().map_err(Error::FindCommit)?;
    for id in tips {
        if !odb.exists(*id) {
            return Err(Error::MissingFetchedCommit(*id));
        }
    }
    Ok(())
}

// Open the shared repository when this checkout is a linked worktree.
fn common_repository(repo: &Repository) -> Result<Option<Repository>, Error> {
    if repo.is_worktree() {
        Repository::open(repo.commondir())
            .map(Some)
            .map_err(Error::DiscoverRepository)
    } else {
        Ok(None)
    }
}

// Resolve a revision such as HEAD or a SHA to a commit.
fn resolve<'repo>(repo: &'repo Repository, revision: &str) -> Result<git2::Commit<'repo>, Error> {
    repo.revparse_single(revision)
        .map_err(Error::ParseRevision)?
        .peel_to_commit()
        .map_err(Error::PeelToCommit)
}

// Return a commit ID, following tags if necessary.
fn peel(object: &Object<'_>) -> Result<Oid, Error> {
    object
        .peel_to_commit()
        .map(|commit| commit.id())
        .map_err(Error::PeelToCommit)
}
