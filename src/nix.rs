use std::process::Command;

use crate::{CommitId, Error, Repository, mutants};

// Run the commit's flake checks and preserve Nix's output validation.
pub(super) fn check_flake(repository: &mut Repository, commit: &CommitId) -> Result<(), Error> {
    // A flake that declares a `mutants-diff` input gets this commit's diff
    // in its place, so cargo-mutants only mutates what the commit changed.
    // Preparing it may fetch the commit's parent into a shallow clone.
    let diff = mutants::prepare_diff(&mut repository.git_repo, commit)?;
    // Nix resolves this against the repository root and encodes the path.
    let mut reference = format!("git+file:./?rev={commit}");
    if repository.git_repo.is_shallow() {
        reference.push_str("&shallow=1");
    }
    let mut command = Command::new("nix");
    command
        .args([
            "flake",
            "check",
            "--no-update-lock-file",
            "--print-build-logs",
        ])
        .current_dir(&repository.root);
    // Nix copies the diff file into its store itself, named `source` so
    // identical diffs reuse earlier results, and keeps it for the whole
    // check.
    if let Some(diff) = &diff {
        command
            .args(["--override-input", "mutants-diff"])
            .arg(format!("path:{}", diff.path().display()));
    }
    let status = command
        .arg(&reference)
        .status()
        .map_err(|source| Error::SpawnNix {
            commit: commit.clone(),
            source,
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::ValidationFailed {
            commit: commit.clone(),
            status,
        })
    }
}
