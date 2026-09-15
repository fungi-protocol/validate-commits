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
