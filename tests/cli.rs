#![cfg(unix)]

use git2::build::TreeUpdateBuilder;
use git2::{FileMode, Oid, Repository};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// Every fixture commit uses this flake; `script` decides whether it passes.
const FLAKE: &[u8] = include_bytes!("flakes/flake.nix");
const PASSING: &str = r#"printf passed > "$out""#;
const FAILING: &str = "echo fixture-check-failed >&2; exit 1";
// A flake whose mutation check runs `cargo mutants --list` on its input.
const MUTANTS_FLAKE: &[u8] = include_bytes!("flakes/mutants/flake.nix");
const MUTANTS_SCRIPT: &[u8] = include_bytes!("flakes/mutants/mutants.sh");
const CRATE_MANIFEST: &[u8] =
    b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

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

    // Commit `files` on top of HEAD, keeping the files earlier commits added.
    fn commit_files(&mut self, message: &str, files: &[(&str, &[u8])]) -> Oid {
        let parent = match self.repository.head() {
            Ok(head) => Some(head.peel_to_commit().unwrap()),
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => None,
            Err(error) => panic!("{error}"),
        };
        let base = match &parent {
            Some(commit) => commit.tree().unwrap(),
            None => {
                let empty = self.repository.treebuilder(None).unwrap().write().unwrap();
                self.repository.find_tree(empty).unwrap()
            }
        };
        let mut update = TreeUpdateBuilder::new();
        for (name, content) in files {
            let path = self.repo.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            let blob = self.repository.blob(content).unwrap();
            update.upsert(*name, blob, FileMode::Blob);
        }
        let tree = update.create_updated(&self.repository, &base).unwrap();
        let tree = self.repository.find_tree(tree).unwrap();
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
        self.commit_files(
            message,
            &[
                ("flake.nix", FLAKE),
                ("builder", host_shell().as_bytes()),
                ("script", script.as_bytes()),
            ],
        )
    }

    fn run(&self, revisions: &str) -> Output {
        self.run_args(&[revisions])
    }

    fn run_args(&self, revisions: &[&str]) -> Output {
        self.with_private_nix(Command::new(env!("CARGO_BIN_EXE_validate-commits")))
            .args(revisions)
            // Include CLI subprocesses in the coverage collector's output.
            .envs(env::var_os("LLVM_PROFILE_FILE").map(|path| ("LLVM_PROFILE_FILE", path)))
            .output()
            .unwrap()
    }

    // Lock the flake at HEAD and commit its flake.lock. The fixtures commit
    // without updating Git's index, so lock the commit, not the checkout.
    fn lock(&mut self) -> Oid {
        let head = self.repository.head().unwrap().target().unwrap();
        let lock = self.root.path().join("flake.lock");
        let output = self
            .with_private_nix(Command::new("nix"))
            .args(["flake", "lock", "--output-lock-file"])
            .arg(&lock)
            .arg(format!("git+file:.?rev={head}"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.commit_files(
            "Lock the flake",
            &[("flake.lock", &fs::read(lock).unwrap())],
        )
    }

    // Build tiny dependency-free checks in a private single-user store.
    // No daemon, downloads, privileged sandbox, or user store is needed.
    fn with_private_nix(&self, mut command: Command) -> Command {
        let root = self.root.path().display();
        let config = format!(
            "store = local?store={root}/store&state={root}/state&log={root}/log\n\
             sandbox = false\n\
             build-users-group =\n\
             substituters =\n\
             flake-registry =\n\
             experimental-features = nix-command flakes\n"
        );
        command
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap())
            .env("NIX_CONFIG", config)
            .env("NIX_USER_CONF_FILES", "")
            .env("XDG_CACHE_HOME", self.root.path().join("cache"));
        command
    }
}

fn host_shell() -> String {
    let shell = env::split_paths(&env::var_os("PATH").unwrap())
        .map(|directory| directory.join("sh"))
        .find(|candidate| candidate.is_file())
        .expect("sh must be available while running tests")
        .canonicalize()
        .unwrap();
    shell.to_str().unwrap().to_owned()
}

// Shell lines that let a fixture derivation run the host's cargo and
// cargo-mutants, the same way the fixtures run the host's sh. rustup's
// cargo also needs to find its toolchains.
fn host_tools() -> String {
    let mut tools = format!("export PATH='{}'\n", env::var("PATH").unwrap());
    for name in ["RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        if let Ok(value) = env::var(name) {
            tools.push_str(&format!("export {name}='{value}'\n"));
        }
    }
    tools
}

fn crate_source(changed: i32) -> Vec<u8> {
    format!(
        "pub fn changed() -> i32 {{\n    {changed}\n}}\n\npub fn untouched() -> i32 {{\n    2\n}}\n"
    )
    .into_bytes()
}

fn git(directory: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("git must be on PATH to run the shallow clone test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    let output = fixture.run(&format!("{base}..HEAD"));
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
fn checks_commits_in_a_shallow_clone() {
    // Like a CI checkout: clone only the newest commit, then validate a
    // range whose base is not in the clone. The tool must fetch enough
    // history, and must tell Nix the clone is shallow or Nix refuses it.
    let mut fixture = Fixture::new();
    fixture.flake_commit("Flake", PASSING);
    for i in 0..10 {
        fixture.commit(&format!("Old {i}"));
    }
    let base = fixture.commit("Base");
    let head = fixture.commit("Head");
    // Let the clone fetch any commit by its ID, as GitHub allows.
    git(
        &fixture.repo,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
    let url = format!("file://{}", fixture.repo.display());
    git(
        fixture.root.path(),
        &["clone", "--quiet", "--depth=1", &url, "clone"],
    );
    fixture.repo = fixture.root.path().join("clone");
    fixture.repository = Repository::open(&fixture.repo).unwrap();

    let output = fixture.run(&format!("{base}..HEAD"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(validated(&output), [head]);
    assert!(Repository::open(&fixture.repo).unwrap().is_shallow());
}

#[test]
fn passes_each_commits_rust_diff_to_cargo_mutants() {
    // A crate with two functions. The commit under test changes one of
    // them and adds two files with odd names: one with a space, quotes, a
    // backslash and non-ASCII letters, and one with a tab. The test flake
    // runs `cargo mutants --list` on the diff this tool gives it. It fails
    // if any mutant is outside the changed function and the new files.
    Command::new("cargo-mutants")
        .arg("--version")
        .output()
        .expect("cargo-mutants must be on PATH to run the mutation test");
    let mut fixture = Fixture::new();
    fixture.commit_files(
        "Crate with a mutation check",
        &[
            ("flake.nix", MUTANTS_FLAKE),
            ("mutants.sh", MUTANTS_SCRIPT),
            ("builder", host_shell().as_bytes()),
            ("tools", host_tools().as_bytes()),
            ("mutants-diff/full-suite", b""),
            ("Cargo.toml", CRATE_MANIFEST),
            ("src/lib.rs", &crate_source(1)),
        ],
    );
    let base = fixture.lock();
    let module = br#"#[path = "\u{fc}n\u{ef} \"q\" sp\\added.rs"]
pub mod added;
#[path = "with\ttab.rs"]
pub mod tab;
"#;
    let lib = [module.as_slice(), &crate_source(10)].concat();
    let head = fixture.commit_files(
        "Change one function and add modules",
        &[
            ("src/lib.rs", &lib),
            (
                "src/ünï \"q\" sp\\added.rs",
                b"pub fn added() -> i32 {\n    3\n}\n",
            ),
            ("src/with\ttab.rs", b"pub fn added() -> i32 {\n    4\n}\n"),
        ],
    );

    let output = fixture.run(&format!("{base}..HEAD"));
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{log}");
    assert_eq!(validated(&output), [head]);
    // Nix prints the check's log. Make sure cargo-mutants found mutants in
    // the changed function and in both new files.
    assert!(log.contains("replace changed"), "{log}");
    assert!(log.contains("added.rs:2:5: replace added"), "{log}");
    assert!(log.contains("tab.rs:2:5: replace added"), "{log}");
}
