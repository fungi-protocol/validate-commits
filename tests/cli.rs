#![cfg(unix)]

use gix::{ObjectId, Repository};
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
        let repository = with_test_identity(gix::init(&repo).unwrap());
        Self {
            root,
            repo,
            repository,
        }
    }

    fn commit(&mut self, message: &str) -> ObjectId {
        self.commit_files(message, &[("tracked", message.as_bytes())])
    }

    // Commit `files` on top of HEAD, keeping the files earlier commits added.
    fn commit_files(&mut self, message: &str, files: &[(&str, &[u8])]) -> ObjectId {
        let parent = self.repository.head_commit().ok();
        let tree = match &parent {
            Some(commit) => commit.tree_id().unwrap().detach(),
            None => self.repository.empty_tree().id,
        };
        let mut tree = self.repository.edit_tree(tree).unwrap();
        for (name, content) in files {
            let path = self.repo.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            let blob = self.repository.write_blob(content).unwrap();
            tree.upsert(*name, gix::object::tree::EntryKind::Blob, blob)
                .unwrap();
        }
        let tree = tree.write().unwrap();
        self.repository
            .commit("HEAD", message, tree, parent.map(|commit| commit.id))
            .unwrap()
            .detach()
    }

    // The fixture flake uses builtins.derivation without nixpkgs, so checks
    // build offline in milliseconds. It still needs a shell as its builder,
    // and pure flake evaluation cannot read environment variables, so the
    // host's sh path is committed as `builder` for the flake to readFile.
    fn flake_commit(&mut self, message: &str, script: &str) -> ObjectId {
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
    fn lock(&mut self) -> ObjectId {
        let head = self.repository.head_id().unwrap();
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

// Fixture commits need an author and committer, and updating branches
// writes a reflog entry naming the committer. Set a user in memory, which
// gix uses for both, so the tests also run where Git has no user config,
// like the Nix build sandbox.
fn with_test_identity(mut repository: Repository) -> Repository {
    let mut config = repository.config_snapshot_mut();
    config
        .set_value(&gix::config::tree::User::NAME, "Test")
        .unwrap();
    config
        .set_value(&gix::config::tree::User::EMAIL, "test@example.com")
        .unwrap();
    config.commit().unwrap();
    repository
}

fn validated(output: &Output) -> Vec<ObjectId> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("Validating "))
        .map(|id| ObjectId::from_hex(id.as_bytes()).unwrap())
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
    let head = fixture.repository.head_id().unwrap().detach();
    fs::write(fixture.repo.join("flake.nix"), "uncommitted flake edit").unwrap();
    let output = fixture.run(&format!("{base}..{second}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(validated(&output), [first, second]);
    assert_eq!(fixture.repository.head_id().unwrap().detach(), head);
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
    fixture.repository = gix::open(&fixture.repo).unwrap();

    let output = fixture.run(&format!("{base}..HEAD"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(validated(&output), [head]);
    assert!(fixture.repository.is_shallow().unwrap());
}

#[test]
fn passes_each_commits_rust_diff_to_cargo_mutants() {
    // A crate with two functions, then a commit that changes one and adds a
    // module. The fixture flake runs `cargo mutants --list` on the diff the
    // tool passes in, and fails unless every mutant comes from the changed
    // function or the new module.
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
    let lib = [b"pub mod added;\n".as_slice(), &crate_source(10)].concat();
    let head = fixture.commit_files(
        "Change one function and add a module",
        &[
            ("src/lib.rs", &lib),
            ("src/added.rs", b"pub fn added() -> i32 {\n    3\n}\n"),
        ],
    );

    let output = fixture.run(&format!("{base}..HEAD"));
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{log}");
    assert_eq!(validated(&output), [head]);
    // Nix prints the check's log, so cargo-mutants really listed mutants,
    // including ones in the new file.
    assert!(log.contains("replace changed"), "{log}");
    assert!(log.contains("replace added"), "{log}");
}
