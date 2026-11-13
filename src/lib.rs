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
    let repository = discover_repository()?;
    let commits = history::resolve_commits(&repository.git_repo, revisions, output)?;

    if commits.is_empty() {
        writeln!(output, "No commits in range.").map_err(Error::WriteOutput)?;
        return Ok(());
    }

    for commit in &commits {
        writeln!(output, "Validating {commit}").map_err(Error::WriteOutput)?;
        output.flush().map_err(Error::WriteOutput)?;
        nix::check_flake(&repository, commit)?;
    }

    Ok(())
}

struct Repository {
    git_repo: gix::Repository,
    root: PathBuf,
}

fn discover_repository() -> Result<Repository, Error> {
    let mut git_repo = gix::discover(".").context(Error::DiscoverRepository)?;
    // Commits are looked up again after walking; caching them is cheap.
    git_repo.object_cache_size_if_unset(4 * 1024 * 1024);
    let root = git_repo
        .workdir()
        .ok_or(Error::BareRepository)?
        .canonicalize()
        .map_err(Error::CanonicalizeRepository)?;
    Ok(Repository { git_repo, root })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitId(gix::ObjectId);

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

// gix returns a different error type from each operation, so errors keep
// their source as a boxed error.
type Source = Box<dyn std::error::Error + Send + Sync + 'static>;

// Wrap any gix error in one of our variants: `.context(Error::WalkHistory)?`.
trait Context<T> {
    fn context(self, variant: fn(Source) -> Error) -> Result<T, Error>;
}

impl<T, E: Into<Source>> Context<T> for Result<T, E> {
    fn context(self, variant: fn(Source) -> Error) -> Result<T, Error> {
        self.map_err(|error| variant(error.into()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not discover the Git repository: {0}")]
    DiscoverRepository(#[source] Source),
    #[error("cannot validate a bare Git repository")]
    BareRepository,
    #[error("could not resolve the repository path: {0}")]
    CanonicalizeRepository(#[source] std::io::Error),
    #[error("could not parse the revision: {0}")]
    ParseRevision(#[source] Source),
    #[error("could not resolve a selected commit: {0}")]
    PeelToCommit(#[source] Source),
    #[error("could not walk the selected history: {0}")]
    WalkHistory(#[source] Source),
    #[error("could not read a selected commit: {0}")]
    FindCommit(#[source] Source),
    #[error("only X, ^X and A..B revisions are supported")]
    UnsupportedRevision,
    #[error("could not read shallow history: {0}")]
    ReadShallow(#[source] Source),
    #[error("could not run git fetch: {0}")]
    SpawnGit(#[source] std::io::Error),
    #[error("git fetch failed: {status}")]
    FetchFailed { status: ExitStatus },
    #[error("fetch did not supply requested commit {0}")]
    MissingFetchedCommit(gix::ObjectId),
    #[error("fetch did not deepen history; origin may itself be shallow")]
    NoHistoryProgress,
    #[error("could not prepare the mutation diff for commit {commit}: {source}")]
    MutationDiff { commit: CommitId, source: Source },
    #[error("could not parse flake.lock for commit {commit}: {source}")]
    ParseLock {
        commit: CommitId,
        source: serde_json::Error,
    },
    #[error("could not write the mutation diff: {0}")]
    WriteMutationDiff(#[source] std::io::Error),
    #[error("could not add the mutation diff for commit {commit} to the Nix store: {status}")]
    ImportMutationDiff {
        commit: CommitId,
        status: ExitStatus,
    },
    #[error("Nix returned an invalid store path for the mutation diff")]
    InvalidDiffStorePath,
    #[error("could not write output: {0}")]
    WriteOutput(#[source] std::io::Error),
    #[error("could not start Nix while checking commit {commit}: {source}")]
    SpawnNix {
        commit: CommitId,
        source: std::io::Error,
    },
    #[error("flake checks failed for commit {commit}: {status}")]
    ValidationFailed {
        commit: CommitId,
        status: ExitStatus,
    },
}
