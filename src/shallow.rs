// Shallow clones: find where downloaded history stops and fetch more of it.

use gix::{ObjectId, Repository};
use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::{Context, Error};

// Return the commit a full commit ID names if this clone does not have it
// yet, so it can be fetched. Abbreviated IDs and expressions such as SHA~1
// are never fetched.
pub(crate) fn missing_commit(repo: &Repository, revision: &str) -> Option<ObjectId> {
    // SHA-1 and SHA-256 repositories use IDs of different lengths.
    if revision.len() != repo.object_hash().len_in_hex() {
        return None;
    }
    let id = ObjectId::from_hex(revision.as_bytes()).ok()?;
    (!repo.has_object(id)).then_some(id)
}

// Check whether a commit originally had parents. gix reports a commit's
// real parents even when this clone cut them off.
fn has_parents(repo: &Repository, id: ObjectId) -> Result<bool, Error> {
    let commit = repo.find_commit(id).context(Error::FindCommit)?;
    Ok(commit.parent_ids().next().is_some())
}

// The commits where this clone's downloaded history stops. gix reads them
// from the shared repository, so linked worktrees need no special case.
pub(crate) fn boundaries(repo: &Repository) -> Result<Vec<ObjectId>, Error> {
    let commits = repo.shallow_commits().context(Error::ReadShallow)?;
    Ok(commits
        .map(|commits| commits.iter().copied().collect())
        .unwrap_or_default())
}

// The boundary commits whose parents exist on origin but not here. Real
// root commits have no parents to fetch.
pub(crate) fn cutoffs(repo: &Repository) -> Result<HashSet<ObjectId>, Error> {
    let mut cutoffs = HashSet::new();
    for id in boundaries(repo)? {
        if has_parents(repo, id)? {
            cutoffs.insert(id);
        }
    }
    Ok(cutoffs)
}

// Fetch commits or more ancestry without changing branches or staged files.
pub(crate) fn fetch(
    repo: &Repository,
    tips: &[ObjectId],
    deepen: Option<u32>,
    output: &mut impl Write,
) -> Result<(), Error> {
    let before = boundaries(repo)?;
    let depth_flag = match deepen {
        Some(count) => {
            writeln!(output, "Deepening history by {count} commits").map_err(Error::WriteOutput)?;
            output.flush().map_err(Error::WriteOutput)?;
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
    // gix notices the new objects and shallow boundaries without reopening.
    if deepen.is_some() && boundaries(repo)? == before {
        return Err(Error::NoHistoryProgress);
    }
    for id in tips {
        if !repo.has_object(id) {
            return Err(Error::MissingFetchedCommit(*id));
        }
    }
    Ok(())
}
