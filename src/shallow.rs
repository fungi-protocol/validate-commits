// Shallow clones: find where downloaded history stops and fetch more of it.
//
// libgit2 reads the clone's `shallow` file as grafts, so a commit where
// history stops looks like it has no parents. That keeps its history walk
// working, but it has no API that lists those commits or their real
// parents, so both are read here.

use git2::{Commit, Oid, Repository};
use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::Error;

// Return the commit a full commit ID names if this clone does not have it
// yet, so it can be fetched. Abbreviated IDs and expressions such as SHA~1
// are never fetched.
pub(crate) fn missing_commit(repo: &Repository, revision: &str) -> Option<Oid> {
    if revision.len() != 40 {
        return None;
    }
    let id = Oid::from_str(revision).ok()?;
    let odb = repo.odb().ok()?;
    (!odb.exists(id)).then_some(id)
}

// A commit's parents as recorded in the commit itself. libgit2's
// `parent_ids` leaves out parents a shallow clone cut off.
pub(crate) fn real_parents(commit: &Commit<'_>) -> Vec<Oid> {
    commit
        .raw_header_bytes()
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"parent "))
        .filter_map(|id| std::str::from_utf8(id).ok())
        .filter_map(|id| Oid::from_str(id).ok())
        .collect()
}

// The commits where this clone's downloaded history stops, from the
// repository's `shallow` file. Linked worktrees share it.
fn boundaries(repo: &Repository) -> Result<Vec<Oid>, Error> {
    match std::fs::read_to_string(repo.commondir().join("shallow")) {
        Ok(contents) => Ok(contents
            .lines()
            .filter_map(|line| Oid::from_str(line).ok())
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(Error::ReadShallow(error)),
    }
}

// The boundary commits whose parents exist on origin but not here. Real
// root commits have no parents to fetch.
pub(crate) fn cutoffs(repo: &Repository) -> Result<HashSet<Oid>, Error> {
    let mut cutoffs = HashSet::new();
    for id in boundaries(repo)? {
        let commit = repo.find_commit(id).map_err(Error::FindCommit)?;
        if !real_parents(&commit).is_empty() {
            cutoffs.insert(id);
        }
    }
    Ok(cutoffs)
}

// Fetch commits or more ancestry without changing branches or staged files,
// then reopen the repository: libgit2 reads the `shallow` file and caches
// commits when it opens a repository, so it would not see the new history.
//
// This runs `git fetch` rather than libgit2's fetch. libgit2 refuses
// shallow fetches from local repositories, and it ignores the
// `http.extraheader` setting that GitHub's checkout action uses to log in.
pub(crate) fn fetch(
    repo: &mut Repository,
    tips: &[Oid],
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
    *repo = Repository::open(repo.path()).map_err(Error::ReopenRepository)?;
    if deepen.is_some() && boundaries(repo)? == before {
        return Err(Error::NoHistoryProgress);
    }
    let odb = repo.odb().map_err(Error::ReopenRepository)?;
    for id in tips {
        if !odb.exists(*id) {
            return Err(Error::MissingFetchedCommit(*id));
        }
    }
    Ok(())
}
