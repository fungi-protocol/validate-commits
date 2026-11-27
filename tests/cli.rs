#![cfg(unix)]

use git2::{Oid, Repository};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// Every fixture commit uses this flake; `script` decides whether it passes.
const FLAKE: &[u8] = include_bytes!("flakes/flake.nix");
const PASSING: &str = r#"printf passed > "$out""#;
const FAILING: &str = "echo fixture-check-failed >&2; exit 1";

struct Fixture {
    root: TempDir,
    repo: PathBuf,
    repository: Repository,
}

impl Fixture {
    fn new() -> Self {
        Self::with_repo_name("repo")
    }

    fn with_repo_name(name: &str) -> Self {
        Command::new("nix")
            .arg("--version")
            .output()
            .expect("nix must be on PATH to run the CLI tests");
        let root = tempfile::Builder::new()
            .prefix("validate-commits.")
            .tempdir()
            .unwrap();
        let repo = root.path().join(name);
        let repository = Repository::init(&repo).unwrap();
        Self {
            root,
            repo,
            repository,
        }
    }

    fn commit(&mut self, message: &str) -> Oid {
        self.commit_files(message, &[("tracked", message.as_bytes())])
    }

    fn commit_files(&mut self, message: &str, files: &[(&str, &[u8])]) -> Oid {
        let parent = match self.repository.head() {
            Ok(head) => Some(head.peel_to_commit().unwrap()),
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => None,
            Err(error) => panic!("{error}"),
        };
        let previous = parent.as_ref().map(|commit| commit.tree().unwrap());
        let mut tree = self.repository.treebuilder(previous.as_ref()).unwrap();
        for (name, content) in files {
            fs::write(self.repo.join(name), content).unwrap();
            let blob = self.repository.blob(content).unwrap();
            tree.insert(*name, blob, 0o100644).unwrap();
        }
        let tree = self.repository.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        self.repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parent.iter().collect::<Vec<_>>(),
            )
            .unwrap()
    }

    // The fixture flake uses builtins.derivation without nixpkgs, so checks
    // build offline in milliseconds. It still needs a shell as its builder,
    // and pure flake evaluation cannot read environment variables, so the
    // host's sh path is committed as `builder` for the flake to readFile.
    fn flake_commit(&mut self, message: &str, script: &str) -> Oid {
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|directory| directory.join("sh"))
            .find(|candidate| candidate.is_file())
            .expect("sh must be available while running tests")
            .canonicalize()
            .unwrap();
        self.commit_files(
            message,
            &[
                ("flake.nix", FLAKE),
                ("builder", shell.to_str().unwrap().as_bytes()),
                ("script", script.as_bytes()),
            ],
        )
    }

    fn run(&self, revisions: &str) -> Output {
        self.run_args(&[revisions])
    }

    fn run_args(&self, revisions: &[&str]) -> Output {
        let root = self.root.path().display();
        // Build tiny dependency-free checks in a private single-user store.
        // No daemon, downloads, privileged sandbox, or user store is needed.
        let config = format!(
            "store = local?store={root}/store&state={root}/state&log={root}/log\n\
             sandbox = false\n\
             build-users-group =\n\
             substituters =\n\
             flake-registry =\n\
             experimental-features = nix-command flakes\n"
        );
        Command::new(env!("CARGO_BIN_EXE_validate-commits"))
            .args(revisions)
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap())
            .env("NIX_CONFIG", config)
            .env("NIX_USER_CONF_FILES", "")
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            // Include CLI subprocesses in the coverage collector's output.
            .envs(env::var_os("LLVM_PROFILE_FILE").map(|path| ("LLVM_PROFILE_FILE", path)))
            .output()
            .unwrap()
    }
}

fn validated(output: &Output) -> Vec<Oid> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("Validating "))
        .map(|id| Oid::from_str(id).unwrap())
        .collect()
}

#[test]
fn checks_selected_commit_flakes_without_changing_the_checkout() {
    // URL metacharacters in the path ensure the flake reference stays
    // relative to the repository instead of embedding its absolute path.
    let mut fixture = Fixture::with_repo_name("repo[a]^b|c");
    let base = fixture.flake_commit("Base flake", PASSING);
    let first = fixture.commit("First");
    let second = fixture.commit("Second");
    // The checkout fails, but the two selected historical flakes pass.
    fixture.flake_commit("Failing tip", FAILING);
    let head = fixture.repository.head().unwrap().target().unwrap();
    fs::write(fixture.repo.join("flake.nix"), "uncommitted flake edit").unwrap();
    let output = fixture.run(&format!("{base}..{second}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(validated(&output), [first, second]);
    assert_eq!(fixture.repository.head().unwrap().target().unwrap(), head);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("flake.nix")).unwrap(),
        "uncommitted flake edit"
    );
}

#[test]
fn stops_when_a_selected_commits_check_fails() {
    let mut fixture = Fixture::new();
    let base = fixture.flake_commit("Base flake", PASSING);
    let passing = fixture.commit("Passing commit");
    let failing = fixture.flake_commit("Failing commit", FAILING);
    fixture.flake_commit("Passing tip", PASSING);
    let output = fixture.run_args(&["--all", &format!("^{base}")]);
    assert!(!output.status.success());
    assert_eq!(validated(&output), [passing, failing]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("fixture-check-failed"), "{error}");
    assert!(
        error.contains(&format!("flake checks failed for commit {failing}")),
        "{error}"
    );
}

#[test]
fn checks_a_commit_at_a_shallow_boundary() {
    // Mimic a depth-1 clone: HEAD is a shallow boundary whose parent
    // object is missing from the repository.
    let mut fixture = Fixture::new();
    let parent = fixture.flake_commit("Parent", PASSING);
    let head = fixture.commit("Head");
    let objects = fixture.repository.path().join("objects");
    let parent = parent.to_string();
    fs::remove_file(objects.join(&parent[..2]).join(&parent[2..])).unwrap();
    let shallow = fixture.repository.commondir().join("shallow");
    fs::write(&shallow, format!("{head}\n")).unwrap();
    let output = fixture.run("HEAD^!");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(validated(&output), [head]);
}
