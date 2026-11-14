mod history;
mod mutants;
mod nix;
mod shallow;
#[cfg(test)]
mod test_support;

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitStatus;

pub fn run_revisions(revisions: &[&str], output: &mut impl Write) -> Result<(), Error> {
    let mut repository = discover_repository()?;
    let commits = history::resolve_commits(&mut repository.git_repo, revisions, output)?;

    if commits.is_empty() {
        writeln!(output, "No commits in range.").map_err(Error::WriteOutput)?;
        return Ok(());
    }

    for commit in &commits {
        writeln!(output, "Validating {commit}").map_err(Error::WriteOutput)?;
        output.flush().map_err(Error::WriteOutput)?;
        nix::check_flake(&mut repository, commit)?;
    }

    Ok(())
}

struct Repository {
    git_repo: git2::Repository,
    root: PathBuf,
}

fn discover_repository() -> Result<Repository, Error> {
    let git_repo = git2::Repository::discover(".").map_err(Error::DiscoverRepository)?;
    let root = git_repo
        .workdir()
        .ok_or(Error::BareRepository)?
        .canonicalize()
        .map_err(Error::CanonicalizeRepository)?;
    Ok(Repository { git_repo, root })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitId(git2::Oid);

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not discover the Git repository: {0}")]
    DiscoverRepository(#[source] git2::Error),
    #[error("cannot validate a bare Git repository")]
    BareRepository,
    #[error("could not resolve the repository path: {0}")]
    CanonicalizeRepository(#[source] std::io::Error),
    #[error("could not parse the revision: {0}")]
    ParseRevision(#[source] git2::Error),
    #[error("only X and A..B revisions are supported")]
    UnsupportedRevision,
    #[error("could not resolve a selected commit: {0}")]
    PeelToCommit(#[source] git2::Error),
    #[error("could not walk the selected history: {0}")]
    WalkHistory(#[source] git2::Error),
    #[error("could not read shallow history: {0}")]
    ReadShallow(#[source] std::io::Error),
    #[error("could not read a commit: {0}")]
    FindCommit(#[source] git2::Error),
    #[error("could not run git fetch: {0}")]
    SpawnGit(#[source] std::io::Error),
    #[error("git fetch failed{}", status_suffix(.status))]
    FetchFailed { status: ExitStatus },
    #[error("could not reopen the repository after fetching: {0}")]
    ReopenRepository(#[source] git2::Error),
    #[error("fetch did not supply requested commit {0}")]
    MissingFetchedCommit(git2::Oid),
    #[error("fetch did not deepen history; origin may itself be shallow")]
    NoHistoryProgress,
    #[error("could not prepare the mutation diff for commit {commit}: {source}")]
    MutationDiff {
        commit: CommitId,
        source: git2::Error,
    },
    #[error("could not parse flake.lock for commit {commit}: {source}")]
    ParseLock {
        commit: CommitId,
        source: serde_json::Error,
    },
    #[error("could not write the mutation diff: {0}")]
    WriteMutationDiff(#[source] std::io::Error),
    #[error("the mutants-diff input in commit {commit} must be declared with `flake = false`")]
    MutantsDiffIsFlake { commit: CommitId },
    #[error("could not write output: {0}")]
    WriteOutput(#[source] std::io::Error),
    #[error("could not start Nix while checking commit {commit}: {source}")]
    SpawnNix {
        commit: CommitId,
        source: std::io::Error,
    },
    #[error("flake checks failed for commit {commit}{}", status_suffix(.status))]
    ValidationFailed {
        commit: CommitId,
        status: ExitStatus,
    },
}

fn status_suffix(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!(" with exit status {code}"),
        None => " after termination by signal".to_owned(),
    }
}
