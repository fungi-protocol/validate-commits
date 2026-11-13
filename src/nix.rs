use std::process::Command;

use crate::{CommitId, Context, Error, Repository, mutants};

// Run the commit's flake checks and preserve Nix's output validation.
pub(super) fn check_flake(repository: &Repository, commit: &CommitId) -> Result<(), Error> {
    // Nix resolves this against the repository root and encodes the path.
    let mut reference = format!("git+file:./?rev={commit}");
    if repository
        .git_repo
        .is_shallow()
        .context(Error::ReadShallow)?
    {
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
    // A flake that declares a `mutants-diff` input gets this commit's diff
    // in its place, so cargo-mutants only mutates what the commit changed.
    if let Some(diff) = mutants::prepare_diff(&repository.git_repo, commit)? {
        let store_path = add_to_store(repository, commit, diff.path())?;
        command.args([
            "--override-input",
            "mutants-diff",
            &format!("path:{store_path}"),
        ]);
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

// Add the diff to the Nix store under a fixed name, so identical diffs reuse
// earlier build results.
fn add_to_store(
    repository: &Repository,
    commit: &CommitId,
    diff: &std::path::Path,
) -> Result<String, Error> {
    let output = Command::new("nix")
        .args(["store", "add", "--name", "mutants-diff"])
        .arg(diff)
        .current_dir(&repository.root)
        .output()
        .map_err(|source| Error::SpawnNix {
            commit: commit.clone(),
            source,
        })?;
    if !output.status.success() {
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr)
            .map_err(Error::WriteOutput)?;
        return Err(Error::ImportMutationDiff {
            commit: commit.clone(),
            status: output.status,
        });
    }
    let store_path = std::str::from_utf8(&output.stdout)
        .map_err(|_| Error::InvalidDiffStorePath)?
        .trim();
    if !store_path.starts_with('/') || store_path.contains('\n') {
        return Err(Error::InvalidDiffStorePath);
    }
    Ok(store_path.to_owned())
}
