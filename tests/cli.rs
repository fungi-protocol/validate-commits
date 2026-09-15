#![cfg(unix)]

use git2::{Oid, Repository};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// Real libgit2 history exercises revision selection. Only Nix is replaced, so
// these tests can observe requests and force failures without building flakes.
struct Fixture {
    root: TempDir,
    repo: PathBuf,
    repository: Repository,
    commits: Vec<Oid>,
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
        let mut expected_sorted = expected_calls(expected);
        expected_sorted.sort();
        assert_eq!(actual, expected_sorted);
        // Independent branches may appear in either order, but every selected
        // parent must be validated before its selected children.
        for (child, call) in expected.iter().zip(expected_calls(expected)) {
            let child_position = calls.iter().position(|entry| entry == &call).unwrap();
            for parent in self.repository.find_commit(*child).unwrap().parent_ids() {
                let parent_call = expected_calls(&[parent]).pop().unwrap();
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
