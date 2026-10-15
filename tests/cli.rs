#![cfg(unix)]

use git2::{Oid, Repository};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use tempfile::TempDir;

// Real libgit2 history exercises revision selection. Only Nix is replaced, so
// these tests can observe requests and force failures without building flakes.
struct Fixture {
    root: TempDir,
    repo: PathBuf,
    repository: Repository,
    commits: Vec<Oid>,
    daemon: Option<GitDaemon>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_repo_name("repo")
    }

    fn with_repo_name(repo_name: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("validate-commits.")
            .tempdir()
            .unwrap();
        let repo = root.path().join(repo_name);
        let repository = Repository::init(&repo).unwrap();
        let mut fixture = Self {
            root,
            repo,
            repository,
            commits: Vec::new(),
            daemon: None,
        };

        for message in ["Base", "First", "Second", "Third"] {
            fixture.commit(message);
        }

        let bin = fixture.root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|directory| directory.join("sh"))
            .find(|candidate| candidate.is_file())
            .expect("sh must be available while running tests")
            .canonicalize()
            .unwrap();
        let script = format!(
            "#!{}\n{}",
            shell.display(),
            r#"
[ "$#" -eq 5 ] || exit 90
[ "$1" = flake ] && [ "$2" = check ] || exit 91
[ "$3" = --no-update-lock-file ] || exit 92
[ "$4" = --print-build-logs ] || exit 93
printf '%s\n' "$5" >> "$TEST_NIX_LOG"
case "$5" in
  *"rev=$TEST_FAIL_COMMIT" | *"rev=$TEST_FAIL_COMMIT&"*) exit 7 ;;
esac
"#
        );
        fs::write(bin.join("nix"), script).unwrap();
        fs::set_permissions(bin.join("nix"), fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn commit(&mut self, message: &str) {
        fs::write(self.repo.join("tracked"), message).unwrap();
        let blob = self.repository.blob(message.as_bytes()).unwrap();
        let mut tree = self.repository.treebuilder(None).unwrap();
        tree.insert("tracked", blob, 0o100644).unwrap();
        let tree = tree.write().unwrap();
        let parents = self.commits.last().copied();
        let commit = commit_as(
            &self.repository,
            Some("HEAD"),
            message,
            tree,
            parents,
            self.commits.len() as i64,
        );
        self.commits.push(commit);
    }

    fn run(&self, range: &str, fail: Option<Oid>) -> Output {
        self.run_args(&[range], fail)
    }

    fn run_args(&self, args: &[&str], fail: Option<Oid>) -> Output {
        let paths = std::iter::once(self.root.path().join("bin"))
            .chain(env::split_paths(&env::var_os("PATH").unwrap()))
            .collect::<Vec<_>>();
        Command::new(env!("CARGO_BIN_EXE_validate-commits"))
            .args(args)
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", env::join_paths(paths).unwrap())
            .env("TEST_NIX_LOG", self.root.path().join("calls"))
            .env(
                "TEST_FAIL_COMMIT",
                fail.map(|oid| oid.to_string()).unwrap_or_default(),
            )
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<String> {
        let path = self.root.path().join("calls");
        if !path.exists() {
            return Vec::new();
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn expected_calls(&self, commits: &[Oid]) -> Vec<String> {
        let suffix = if fs::read_to_string(self.repository.commondir().join("shallow"))
            .is_ok_and(|text| !text.trim().is_empty())
        {
            "&shallow=1"
        } else {
            ""
        };
        commits
            .iter()
            .map(|commit| format!("git+file:./?rev={commit}{suffix}"))
            .collect()
    }

    fn clear_calls(&self) {
        let path = self.root.path().join("calls");
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }

    fn assert_validates(&self, expected: &[Oid]) {
        let calls = self.calls();
        let mut actual = calls.clone();
        actual.sort();
        let mut expected_sorted = self.expected_calls(expected);
        expected_sorted.sort();
        assert_eq!(actual, expected_sorted);
        // Independent branches may appear in either order, but every selected
        // parent must be validated before its selected children.
        for (child, call) in expected.iter().zip(self.expected_calls(expected)) {
            let child_position = calls.iter().position(|entry| entry == &call).unwrap();
            for parent in self.repository.find_commit(*child).unwrap().parent_ids() {
                let parent_call = self.expected_calls(&[parent]).pop().unwrap();
                if let Some(parent_position) = calls.iter().position(|entry| entry == &parent_call)
                {
                    assert!(
                        parent_position < child_position,
                        "{parent} must precede {child}"
                    );
                }
            }
        }
    }

    fn dirty_checkout(&self) -> (Oid, Vec<u8>, Option<Vec<u8>>) {
        fs::write(self.repo.join("tracked"), "staged edit").unwrap();
        let mut index = self.repository.index().unwrap();
        index.add_path(std::path::Path::new("tracked")).unwrap();
        index.write().unwrap();
        fs::write(self.repo.join("tracked"), "unstaged edit").unwrap();
        fs::write(self.repo.join("untracked"), "keep me").unwrap();
        (
            self.repository.head().unwrap().target().unwrap(),
            fs::read(self.repository.path().join("HEAD")).unwrap(),
            fs::read(self.repository.path().join("index")).ok(),
        )
    }

    fn assert_checkout(&self, before: &(Oid, Vec<u8>, Option<Vec<u8>>)) {
        assert_eq!(self.repository.head().unwrap().target().unwrap(), before.0);
        assert_eq!(
            fs::read(self.repository.path().join("HEAD")).unwrap(),
            before.1
        );
        assert_eq!(
            fs::read(self.repository.path().join("index")).ok(),
            before.2
        );
        assert_eq!(
            fs::read_to_string(self.repo.join("tracked")).unwrap(),
            "unstaged edit"
        );
        assert_eq!(
            fs::read_to_string(self.repo.join("untracked")).unwrap(),
            "keep me"
        );
    }
}

// The CLI hands Nix a relative reference resolved against the repository root
// it sets as working directory, so Nix percent-encodes the repository path
// itself; absolute `git+file:` URLs built here could not express `[`, `]`,
// `^`, or `|` in a path.
fn expected_calls(commits: &[Oid]) -> Vec<String> {
    commits
        .iter()
        .map(|commit| format!("git+file:./?rev={commit}"))
        .collect()
}

fn signature(timestamp: i64) -> git2::Signature<'static> {
    git2::Signature::new("Test", "test@example.com", &git2::Time::new(timestamp, 0)).unwrap()
}

fn commit_as(
    repository: &Repository,
    reference: Option<&str>,
    message: &str,
    tree: Oid,
    parents: impl IntoIterator<Item = Oid>,
    timestamp: i64,
) -> Oid {
    let signature = signature(timestamp);
    let tree = repository.find_tree(tree).unwrap();
    let parents: Vec<_> = parents
        .into_iter()
        .map(|id| repository.find_commit(id).unwrap())
        .collect();
    let parents: Vec<_> = parents.iter().collect();
    repository
        .commit(reference, &signature, &signature, message, &tree, &parents)
        .unwrap()
}

#[test]
fn validates_only_selected_commits_in_order_without_changing_checkout() {
    let fixture = Fixture::with_repo_name("repo[a]^b|c");
    let before = fixture.dirty_checkout();
    let output = fixture.run(&format!("{}..HEAD", fixture.commits[0]), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&fixture.commits[1..]));
    fixture.assert_checkout(&before);
}

#[test]
fn orders_merge_parents_with_skewed_dates() {
    let fixture = Fixture::new();
    let base = fixture.commits[3];
    let tree = fixture.repository.find_commit(base).unwrap().tree_id();
    let commit = |message, parents: &[Oid], timestamp| {
        commit_as(
            &fixture.repository,
            None,
            message,
            tree,
            parents.iter().copied(),
            timestamp,
        )
    };
    // Fork is Left's parent, but its timestamp makes it look newer.
    // Commit relationships must decide the check order.
    let fork = commit("Fork", &[base], 900);
    let left = commit("Left", &[fork], 10);
    let a = commit("A", &[left], 100);
    let b = commit("B", &[a], 100);
    let head = commit("Merge", &[b, fork], 100);

    let output = fixture.run(&format!("{b}...{fork}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[left, a, b]));
    fs::remove_file(fixture.root.path().join("calls")).unwrap();

    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[fork, left, a, b, head]));
}

#[test]
fn combines_multiple_revisions_ranges_and_exclusions() {
    // Main history: Base -> First -> Second -> Third, where HEAD points.
    // Side starts from Base; release points to First.
    let fixture = Fixture::new();
    let base = fixture.commits[0];
    let tree = fixture.repository.find_commit(base).unwrap().tree_id();
    let side = commit_as(&fixture.repository, None, "Side", tree, [base], 100);
    fixture
        .repository
        .reference("refs/heads/side", side, false, "test")
        .unwrap();
    fixture
        .repository
        .reference("refs/heads/base", base, false, "test")
        .unwrap();
    fixture
        .repository
        .reference("refs/heads/release", fixture.commits[1], false, "test")
        .unwrap();
    let before = fixture.dirty_checkout();
    let cases = [
        (vec!["HEAD", "HEAD"], fixture.commits.clone()),
        (
            vec!["HEAD", "side", "^base"],
            vec![
                fixture.commits[1],
                fixture.commits[2],
                fixture.commits[3],
                side,
            ],
        ),
        (
            vec!["base..HEAD", "base..side"],
            vec![
                fixture.commits[1],
                fixture.commits[2],
                fixture.commits[3],
                side,
            ],
        ),
        (
            vec!["HEAD", "^base", "^release"],
            fixture.commits[2..].to_vec(),
        ),
        (
            vec!["^release", "HEAD", "^base"],
            fixture.commits[2..].to_vec(),
        ),
        (vec!["HEAD^@", "^base"], fixture.commits[1..3].to_vec()),
        (vec!["HEAD^!", "side^!"], vec![fixture.commits[3], side]),
        // HEAD^! excludes earlier history, including the commit HEAD~2^! selects.
        (vec!["HEAD^!", "HEAD~2^!"], fixture.commits[3..].to_vec()),
        (vec!["HEAD^!", "^HEAD"], Vec::new()),
        (vec!["HEAD", "missing"], Vec::new()),
    ];
    for (args, expected) in cases {
        let output = fixture.run_args(&args, None);
        if args.contains(&"missing") {
            assert!(!output.status.success());
        } else {
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fixture.assert_validates(&expected);
        fixture.assert_checkout(&before);
        fixture.clear_calls();
    }
}

#[test]
fn all_includes_refs_and_detached_head_with_optional_exclusions() {
    // --all must find commits through branches, tags, and HEAD even when
    // no branch points to the checked-out commit.
    let fixture = Fixture::new();
    let base = fixture.commits[0];
    let tree = fixture.repository.find_commit(base).unwrap().tree_id();
    let side = commit_as(&fixture.repository, None, "Side", tree, [base], 100);
    fixture
        .repository
        .reference("refs/heads/side", side, false, "test")
        .unwrap();
    let tagged = commit_as(&fixture.repository, None, "Tagged", tree, [base], 200);
    fixture
        .repository
        .tag(
            "release",
            &fixture.repository.find_object(tagged, None).unwrap(),
            &signature(200),
            "Release",
            false,
        )
        .unwrap();
    let detached = commit_as(&fixture.repository, None, "Detached", tree, [base], 300);
    fixture.repository.set_head_detached(detached).unwrap();
    // A tag can point to file contents; those should not be checked as a commit.
    let blob = fixture.repository.blob(b"not a commit").unwrap();
    fixture
        .repository
        .tag_lightweight(
            "blob",
            &fixture.repository.find_object(blob, None).unwrap(),
            false,
        )
        .unwrap();
    let before = fixture.dirty_checkout();
    let mut expected = fixture.commits.clone();
    expected.extend([side, tagged, detached]);
    let output = fixture.run_args(&["--all"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_validates(&expected);
    fixture.assert_checkout(&before);
    fixture.clear_calls();

    // Remove detached HEAD and its shared Base, keeping the other history.
    let output = fixture.run_args(&["--all", "^HEAD"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_validates(&[
        fixture.commits[1],
        fixture.commits[2],
        fixture.commits[3],
        side,
        tagged,
    ]);
    fixture.assert_checkout(&before);
}

#[test]
fn symmetric_ranges_hide_all_merge_bases_and_allow_unrelated_histories() {
    // Left and Right both descend from A and B. Both shared ancestors must
    // be excluded, leaving only Left and Right.
    let fixture = Fixture::new();
    let base = fixture.commits[0];
    let tree = fixture.repository.find_commit(base).unwrap().tree_id();
    let a = commit_as(&fixture.repository, None, "A", tree, [base], 100);
    let b = commit_as(&fixture.repository, None, "B", tree, [base], 900);
    let left = commit_as(&fixture.repository, None, "Left", tree, [a, b], 10);
    let right = commit_as(&fixture.repository, None, "Right", tree, [b, a], 20);
    assert_eq!(
        fixture.repository.merge_bases(left, right).unwrap().len(),
        2
    );
    for range in [format!("{left}...{right}"), format!("{right}...{left}")] {
        let output = fixture.run(&range, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.assert_validates(&[left, right]);
        fixture.clear_calls();
    }

    // With no shared ancestor, include both histories without an error.
    let unrelated = commit_as(&fixture.repository, None, "Unrelated", tree, [], 5);
    let output = fixture.run(&format!("HEAD...{unrelated}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected = fixture.commits.clone();
    expected.push(unrelated);
    fixture.assert_validates(&expected);
}

#[test]
fn uses_shallow_mode_only_when_the_repository_is_shallow() {
    let fixture = Fixture::new();
    let head = fixture.commits[3];
    let output = fixture.run("HEAD^!", None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[head]));
    fixture.clear_calls();

    // Mark HEAD as a shallow boundary; the CLI must allow Nix to read it.
    let shallow = fixture.repository.commondir().join("shallow");
    fs::write(&shallow, format!("{head}\n")).unwrap();
    let output = fixture.run("HEAD^!", Some(head));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("with exit status 7"));
    assert_eq!(
        fixture.calls(),
        vec![format!("git+file:./?rev={head}&shallow=1")]
    );
    fixture.clear_calls();

    // After history becomes complete, stop forcing shallow mode.
    fs::remove_file(shallow).unwrap();
    let output = fixture.run("HEAD^!", Some(head));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("with exit status 7"));
    assert_eq!(fixture.calls(), expected_calls(&[head]));
}

fn history_commit(fixture: &Fixture, message: &str, parents: &[Oid]) -> Oid {
    let tree = fixture
        .repository
        .find_commit(fixture.commits[0])
        .unwrap()
        .tree_id();
    commit_as(
        &fixture.repository,
        None,
        message,
        tree,
        parents.iter().copied(),
        100,
    )
}

fn git_at(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct GitDaemon {
    child: Child,
    url: String,
}

impl GitDaemon {
    fn start(root: &std::path::Path) -> Self {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let child = Command::new("git")
            .args(["daemon", "--verbose", "--export-all", "--listen=127.0.0.1"])
            .arg(format!("--port={port}"))
            .arg(format!("--base-path={}", root.display()))
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            url: format!("git://127.0.0.1:{port}"),
        };
        let stderr = daemon.child.stderr.take().unwrap();
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("Ready to rumble"), "{line}");
        // Keep stderr open while the server runs; otherwise later logging can
        // terminate it with a broken pipe. Drain it on a separate thread.
        std::thread::spawn(move || {
            for line in reader.lines() {
                if line.is_err() {
                    break;
                }
            }
        });
        daemon
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn clone_shallow(fixture: &mut Fixture, depth: usize, branch: &str) {
    let remote = fixture.root.path().join("remote");
    fs::rename(&fixture.repo, &remote).unwrap();
    Repository::open(&remote)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("uploadpack.allowAnySHA1InWant", true)
        .unwrap();
    let daemon = GitDaemon::start(fixture.root.path());
    git_at(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            &format!("--depth={depth}"),
            &format!("--branch={branch}"),
            &format!("{}/remote", daemon.url),
            "repo",
        ],
    );
    fixture.repository = Repository::open(&fixture.repo).unwrap();
    fixture.daemon = Some(daemon);
}

fn shallow_fixture(depth: usize) -> (Fixture, Oid, Vec<Oid>) {
    let mut fixture = Fixture::new();
    for i in 0..100 {
        fixture.commit(&format!("Main {i}"));
    }
    let base = *fixture.commits.last().unwrap();
    let mut side = base;
    let mut selected = Vec::new();
    for i in 0..40 {
        side = history_commit(&fixture, &format!("Side {i}"), &[side]);
        selected.push(side);
    }
    let head = history_commit(&fixture, "Merge", &[base, side]);
    selected.push(head);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, depth, "queue");
    (fixture, base, selected)
}

#[test]
fn deepens_missing_base_and_truncated_merge_parents() {
    for depth in [1, 2] {
        let (fixture, base, selected) = shallow_fixture(depth);
        let before = fixture.dirty_checkout();
        let refs = git_at(&fixture.repo, &["show-ref"]);
        let fetch_head = fixture.repo.join(".git/FETCH_HEAD");
        fs::write(&fetch_head, "preserve fetch head\n").unwrap();
        let output = fixture.run(&format!("{base}..HEAD"), None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut actual = fixture.calls();
        let mut expected = fixture.expected_calls(&selected);
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        fixture.assert_checkout(&before);
        assert_eq!(git_at(&fixture.repo, &["show-ref"]), refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "preserve fetch head\n"
        );
    }
}

#[test]
fn complete_and_empty_shallow_ranges_do_not_fetch() {
    let (fixture, base, selected) = shallow_fixture(42);
    git_at(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/validate-commits",
        ],
    );
    assert!(fixture.run("HEAD..HEAD", None).status.success());
    assert!(fixture.calls().is_empty());
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_validates(&selected);
}

#[test]
fn shallow_origin_cannot_supply_history_and_stops_before_nix() {
    for local in [false, true] {
        let (fixture, _, _) = shallow_fixture(1);
        let source = fixture.root.path().join("remote");
        git_at(
            fixture.root.path(),
            &[
                "clone",
                "--quiet",
                "--depth=1",
                "--branch=queue",
                &format!("file://{}", source.display()),
                "shallow-origin",
            ],
        );
        let url = if local {
            fixture
                .root
                .path()
                .join("shallow-origin")
                .display()
                .to_string()
        } else {
            format!("{}/shallow-origin", fixture.daemon.as_ref().unwrap().url)
        };
        fixture.repository.remote_set_url("origin", &url).unwrap();
        let output = fixture.run("HEAD", None);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("git fetch failed") || stderr.contains("fetch did not deepen history"),
            "{stderr}"
        );
        assert!(fixture.calls().is_empty());
    }
}

#[test]
fn fetch_failures_and_invalid_ranges_stop_before_nix() {
    // Depth 1 requires fetching the base; depth 2 requires deepening ancestry.
    for depth in [1, 2] {
        let (fixture, base, _) = shallow_fixture(depth);
        git_at(
            &fixture.repo,
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/validate-commits",
            ],
        );
        let output = fixture.run(&format!("{base}..HEAD"), None);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("git fetch failed"));
        assert!(fixture.calls().is_empty());
        assert!(!fixture.run("missing..HEAD", None).status.success());
        assert!(fixture.calls().is_empty());
    }
}

#[test]
fn deepens_base_history_to_exclude_shared_fork() {
    let mut fixture = Fixture::new();
    for i in 0..100 {
        fixture.commit(&format!("Shared {i}"));
    }
    let shared = *fixture.commits.last().unwrap();
    let fork = history_commit(&fixture, "Fork", &[shared]);
    let branch = history_commit(&fixture, "Branch", &[shared]);
    let mut main = fork;
    for i in 0..20 {
        main = history_commit(&fixture, &format!("Main {i}"), &[main]);
    }
    let base = history_commit(&fixture, "Merge branch", &[main, branch]);
    let pr = history_commit(&fixture, "PR", &[fork]);
    let head = history_commit(&fixture, "Queue", &[base, pr]);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, 1, "queue");
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut actual = fixture.calls();
    actual.sort();
    let mut expected = fixture.expected_calls(&[pr, head]);
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
}

#[test]
fn deepening_can_reach_full_history() {
    let mut fixture = Fixture::new();
    let base = fixture.commits[0];
    let branch = fixture
        .repository
        .head()
        .unwrap()
        .shorthand()
        .unwrap()
        .to_owned();
    clone_shallow(&mut fixture, 1, &branch);
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        fixture.expected_calls(&fixture.commits[1..])
    );
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}

#[test]
fn real_roots_in_complete_ranges_do_not_fetch_unrelated_shallow_history() {
    let fixture = Fixture::new();
    let base = fixture.commits[0];
    let independent = history_commit(&fixture, "Independent root", &[]);
    let head = history_commit(&fixture, "Merge independent history", &[base, independent]);
    let boundary = fixture.commits[3];
    fs::write(fixture.repo.join(".git/shallow"), format!("{boundary}\n")).unwrap();
    // The shallow boundary belongs to another branch; origin is unnecessary.
    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        fixture.expected_calls(&[independent, head])
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".git/shallow")).unwrap(),
        format!("{boundary}\n")
    );
}

#[test]
fn deepening_preserves_independent_roots_in_merge_history() {
    let mut fixture = Fixture::new();
    let base = fixture.commits[0];
    let independent = history_commit(&fixture, "Independent root", &[]);
    let head = history_commit(&fixture, "Merge independent history", &[base, independent]);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, 1, "queue");
    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        fixture.expected_calls(&[independent, head])
    );
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}

#[test]
fn shallow_ranges_without_origin_stop_before_nix() {
    let fixture = Fixture::new();
    let base = fixture.commits[3];
    fs::write(fixture.repo.join(".git/shallow"), format!("{base}\n")).unwrap();
    let output = fixture.run(&format!("{base}..{}", fixture.commits[0]), None);
    assert!(!output.status.success());
    assert!(fixture.calls().is_empty());
}

#[test]
fn orders_merge_parents_with_skewed_dates_in_a_shallow_clone() {
    let mut fixture = Fixture::new();
    for i in 0..20 {
        fixture.commit(&format!("Main {i}"));
    }
    let base = *fixture.commits.last().unwrap();
    let tree = fixture.repository.find_commit(base).unwrap().tree_id();
    let fork = commit_as(&fixture.repository, None, "Fork", tree, [base], 900);
    let left = commit_as(&fixture.repository, None, "Left", tree, [fork], 10);
    let a = history_commit(&fixture, "A", &[left]);
    let b = history_commit(&fixture, "B", &[a]);
    let head = history_commit(&fixture, "Merge", &[b, fork]);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, 6, "queue");
    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        fixture.expected_calls(&[fork, left, a, b, head])
    );
}

#[test]
fn known_single_commit_in_a_shallow_clone_does_not_fetch() {
    let (fixture, _, selected) = shallow_fixture(1);
    git_at(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/validate-commits",
        ],
    );
    let output = fixture.run("HEAD^!", None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        fixture.expected_calls(&[*selected.last().unwrap()])
    );
}

#[test]
fn combined_single_commit_selections_do_not_fetch() {
    let (fixture, _, selected) = shallow_fixture(1);
    let head = *selected.last().unwrap();
    let alias = format!("{head}^!");
    fixture
        .repository
        .remote_set_url("origin", "/nonexistent/validate-commits")
        .unwrap();
    let before = fixture.dirty_checkout();
    for (args, expected) in [
        (vec!["HEAD^!", "^HEAD"], vec![]),
        (vec!["HEAD^@", "^HEAD"], vec![]),
        (vec!["HEAD^!", "HEAD^!"], vec![head]),
        (vec!["HEAD^!", alias.as_str()], vec![head]),
    ] {
        let output = fixture.run_args(&args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.calls(), fixture.expected_calls(&expected));
        fixture.assert_checkout(&before);
        fixture.clear_calls();
    }
}

#[test]
fn local_commits_in_shallow_clones_fetch_ancestor_history() {
    for range in [false, true] {
        let (fixture, base, mut selected) = shallow_fixture(2);
        let head = *selected.last().unwrap();
        let tree = fixture.repository.find_commit(head).unwrap().tree_id();
        // Origin knows the shallow ancestors, but has never seen this local commit.
        let local = commit_as(
            &fixture.repository,
            Some("HEAD"),
            "Local",
            tree,
            [head],
            200,
        );
        let remote = Repository::open(fixture.root.path().join("remote")).unwrap();
        assert!(remote.find_commit(local).is_err());
        selected.push(local);
        let revision = if range {
            format!("{base}..HEAD")
        } else {
            selected.extend(&fixture.commits);
            "HEAD".to_owned()
        };
        let before = fixture.dirty_checkout();
        let refs = git_at(&fixture.repo, &["show-ref"]);
        let output = fixture.run(&revision, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.assert_validates(&selected);
        fixture.assert_checkout(&before);
        assert_eq!(git_at(&fixture.repo, &["show-ref"]), refs);
    }
}

#[test]
fn excluded_shallow_ancestors_do_not_fetch() {
    let (fixture, _, _) = shallow_fixture(2);
    fixture
        .repository
        .remote_set_url("origin", "/nonexistent/validate-commits")
        .unwrap();
    let before = fixture.dirty_checkout();
    let shallow = fs::read(fixture.repository.commondir().join("shallow")).unwrap();
    // Hiding HEAD also hides its shallow parent and that parent's missing ancestry.
    for args in [["HEAD~1^!", "^HEAD"], ["HEAD~1^@", "^HEAD"]] {
        let output = fixture.run_args(&args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.calls().is_empty());
        assert_eq!(
            fs::read(fixture.repository.commondir().join("shallow")).unwrap(),
            shallow
        );
        fixture.assert_checkout(&before);
    }
}

#[test]
fn git_fetch_uses_matching_configured_authentication_headers() {
    let (fixture, base, _) = shallow_fixture(1);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}");
    let mut config = fixture.repository.config().unwrap();
    config
        .set_str("remote.origin.url", &format!("{url}/remote"))
        .unwrap();
    config
        .set_str(
            &format!("http.{url}/.extraheader"),
            "Authorization: Basic test-token",
        )
        .unwrap();
    config
        .set_str(
            &format!("http.{url}/remote-other.extraheader"),
            "X-Wrong-Path: secret",
        )
        .unwrap();
    config
        .set_str(
            "http.http://other.invalid/.extraheader",
            "X-Wrong-Host: secret",
        )
        .unwrap();
    let before = fixture.dirty_checkout();
    let server = std::thread::spawn(move || {
        // Stop after the first request: this test checks header delivery, not Git's HTTP protocol.
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("HTTP request was not received: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            headers.push_str(&line);
        }
        reader
            .get_mut()
            .write_all(
                b"HTTP/1.1 500 Test complete\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        headers
    });
    let output = fixture.run(&format!("{base}..HEAD"), None);
    let headers = server.join().unwrap().to_ascii_lowercase();
    assert!(
        headers.contains("authorization: basic test-token\r\n"),
        "{headers}"
    );
    assert!(!headers.contains("x-wrong-path"));
    assert!(!headers.contains("x-wrong-host"));
    assert!(!output.status.success());
    assert!(fixture.calls().is_empty());
    fixture.assert_checkout(&before);
}

#[test]
fn complete_unrelated_exclusions_do_not_increase_fetch_depth() {
    let (fixture, base, selected) = shallow_fixture(2);
    let remote = Repository::open(fixture.root.path().join("remote")).unwrap();
    let tree = remote.find_commit(base).unwrap().tree_id();
    let mut unrelated = commit_as(&remote, None, "Unrelated root", tree, [], 0);
    for i in 0..300 {
        unrelated = commit_as(&remote, None, "Unrelated history", tree, [unrelated], i);
    }
    // Fetch the unrelated chain completely before asking to exclude it.
    git_at(
        &fixture.repo,
        &[
            "fetch",
            "--no-write-fetch-head",
            "--refmap=",
            "origin",
            &unrelated.to_string(),
        ],
    );
    let before = fixture.dirty_checkout();
    let output = fixture.run_args(&[&format!("{base}..HEAD"), &format!("^{unrelated}")], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_validates(&selected);
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    fixture.assert_checkout(&before);
}

#[test]
fn empty_shallow_selections_do_not_fetch() {
    for revision in ["HEAD...HEAD", "^HEAD", "HEAD.."] {
        let (fixture, _, _) = shallow_fixture(1);
        fixture
            .repository
            .remote_set_url("origin", "/nonexistent/validate-commits")
            .unwrap();
        let output = fixture.run(revision, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.calls().is_empty());
        assert_eq!(
            git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
    }
}

#[test]
fn missing_single_commit_fetches_without_deepening_ancestry() {
    let (fixture, _, selected) = shallow_fixture(1);
    let requested = selected[10];
    let parent = selected[9];
    assert!(!fixture.repository.odb().unwrap().exists(requested));
    let before = fixture.dirty_checkout();

    let output = fixture.run(&format!("{requested}^!"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), fixture.expected_calls(&[requested]));
    // The requested commit is enough; its missing parent must stay unfetched.
    let repo = Repository::open(&fixture.repo).unwrap();
    let odb = repo.odb().unwrap();
    assert!(odb.exists(requested));
    assert!(!odb.exists(parent));
    fixture.assert_checkout(&before);
}

#[test]
fn single_revision_and_parent_selections_deepen_shallow_history() {
    for revision in ["HEAD", "HEAD^@"] {
        let (fixture, _, _) = shallow_fixture(1);
        let remote = fixture.root.path().join("remote");
        let remote_revision = revision.replace("HEAD", "queue");
        let expected: Vec<_> = git_at(&remote, &["rev-list", &remote_revision])
            .lines()
            .map(|line| Oid::from_str(line).unwrap())
            .collect();
        let output = fixture.run(revision, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut actual = fixture.calls();
        actual.sort();
        let mut expected = fixture.expected_calls(&expected);
        expected.sort();
        assert_eq!(actual, expected);
    }
}

#[test]
fn symmetric_shallow_ranges_deepen_until_shared_history_is_excluded() {
    let mut fixture = Fixture::new();
    let shared = fixture.commits[3];
    let mut left = shared;
    let mut right = shared;
    let mut selected = Vec::new();
    for i in 0..3 {
        left = history_commit(&fixture, &format!("Left {i}"), &[left]);
        selected.push(left);
    }
    for i in 0..20 {
        right = history_commit(&fixture, &format!("Right {i}"), &[right]);
        selected.push(right);
    }
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &left.to_string()],
    );
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/right", &right.to_string()],
    );
    clone_shallow(&mut fixture, 1, "queue");
    let output = fixture.run(&format!("HEAD...{right}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut actual = fixture.calls();
    actual.sort();
    let mut expected = fixture.expected_calls(&selected);
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn unavailable_parent_revisions_fail_without_fetching() {
    let (fixture, base, _) = shallow_fixture(1);
    let shallow = fs::read(fixture.repo.join(".git/shallow")).unwrap();
    let before = fixture.dirty_checkout();
    for range in ["HEAD~1..HEAD".to_owned(), format!("{base}~1..HEAD")] {
        let output = fixture.run(&range, None);
        assert!(!output.status.success(), "{range}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Deepening history"), "{range}: {stderr}");
        assert!(fixture.calls().is_empty());
        assert_eq!(
            fs::read(fixture.repo.join(".git/shallow")).unwrap(),
            shallow
        );
        fixture.assert_checkout(&before);
    }
    // The missing explicit SHA was not fetched merely to retry parent notation.
    assert!(!fixture.repo.join(".git/FETCH_HEAD").exists());
    assert!(!fixture.repository.odb().unwrap().exists(base));
}

#[test]
fn local_remotes_preserve_fetch_support_without_changing_refs_or_index() {
    for file_url in [false, true] {
        let (fixture, base, selected) = shallow_fixture(1);
        let remote = fixture.root.path().join("remote");
        let url = if file_url {
            format!("file://{}", remote.display())
        } else {
            remote.display().to_string()
        };
        fixture.repository.remote_set_url("origin", &url).unwrap();
        let before = fixture.dirty_checkout();
        let refs = git_at(&fixture.repo, &["show-ref"]);
        let output = fixture.run(&format!("{base}..HEAD"), None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.assert_validates(&selected);
        fixture.assert_checkout(&before);
        assert_eq!(git_at(&fixture.repo, &["show-ref"]), refs);
    }
}

#[test]
fn relative_deepening_preserves_an_existing_deeper_clone() {
    let (fixture, _, _) = shallow_fixture(60);
    let expected: Vec<_> = git_at(&fixture.root.path().join("remote"), &["rev-list", "queue"])
        .lines()
        .map(|id| Oid::from_str(id).unwrap())
        .collect();
    let output = fixture.run("HEAD", None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Deepening history by 8 commits"));
    fixture.assert_validates(&expected);
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}

#[test]
fn multiple_revisions_and_all_work_when_history_requires_fetching() {
    for all in [false, true] {
        let (fixture, base, selected) = shallow_fixture(1);
        let excluded = format!("^{base}");
        let args = if all {
            vec!["--all"]
        } else {
            vec!["HEAD", "HEAD", excluded.as_str()]
        };
        let expected = if all {
            git_at(&fixture.root.path().join("remote"), &["rev-list", "queue"])
                .lines()
                .map(|id| Oid::from_str(id).unwrap())
                .collect()
        } else {
            selected
        };
        let output = fixture.run_args(&args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.assert_validates(&expected);
    }
}

#[test]
fn linked_worktrees_deepen_shared_history_and_keep_their_staged_changes() {
    let (mut fixture, base, selected) = shallow_fixture(1);
    let linked = fixture.root.path().join("linked");
    git_at(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    fixture.repo = linked;
    fixture.repository = Repository::open(&fixture.repo).unwrap();
    let before = fixture.dirty_checkout();
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_validates(&selected);
    fixture.assert_checkout(&before);
}

#[test]
fn complete_symmetric_ranges_in_linked_worktrees_do_not_fetch() {
    let (mut fixture, base, selected) = shallow_fixture(42);
    let linked = fixture.root.path().join("linked");
    git_at(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    fixture.repo = linked;
    fixture.repository = Repository::open(&fixture.repo).unwrap();
    fixture
        .repository
        .remote_set_url("origin", "/nonexistent/validate-commits")
        .unwrap();
    let output = fixture.run(&format!("HEAD...{base}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Deepening history"));
    fixture.assert_validates(&selected);
}
