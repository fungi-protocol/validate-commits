use std::process::Command;

use crate::{CommitId, Error, Repository, history};

// Run the commit's flake checks and preserve Nix's output validation.
pub(super) fn check_flake(repository: &Repository, commit: &CommitId) -> Result<(), Error> {
    // Nix resolves this against the repository root and encodes the path.
    let mut reference = format!("git+file:./?rev={commit}");
    if history::is_shallow(&repository.git_repo)? {
        reference.push_str("&shallow=1");
    }
    let status = Command::new("nix")
        .args([
            "flake",
            "check",
            "--no-update-lock-file",
            "--print-build-logs",
            &reference,
        ])
        .current_dir(&repository.root)
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
