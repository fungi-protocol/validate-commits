// Per-commit mutation testing: give each commit's flake the diff of that
// commit, so `cargo mutants --in-diff` only mutates code the commit changed.

use git2::{Diff, DiffFormat, DiffLineType, DiffOptions, FileMode, Repository};
use std::io::Write;

use crate::shallow::{fetch, real_parents};
use crate::{CommitId, Error};

// Build the commit's diff against its first parent, if its flake opts in by
// declaring a top-level `mutants-diff` input in its committed flake.lock.
// The diff is written to a temporary file outside the checkout.
pub(crate) fn prepare_diff(
    repo: &mut Repository,
    id: &CommitId,
) -> Result<Option<tempfile::NamedTempFile>, Error> {
    let error = |source| Error::MutationDiff {
        commit: id.clone(),
        source,
    };
    // Merges use their first parent, and a root commit nothing. libgit2
    // hides a parent that a shallow clone cut off, so read it from the
    // commit and fetch it rather than diffing against nothing. In a full
    // clone a missing parent means a broken repository, so let the lookup
    // below report it.
    let (tree, parent) = {
        let commit = repo.find_commit(id.0).map_err(error)?;
        let tree = commit.tree().map_err(error)?;
        if !declares_mutants_diff(repo, &tree, id)? {
            return Ok(None);
        }
        (tree.id(), real_parents(&commit).first().copied())
    };
    if let Some(parent) = parent
        && repo.is_shallow()
        && !repo.odb().map_err(error)?.exists(parent)
    {
        fetch(repo, &[parent], None, &mut std::io::sink())?;
    }
    let old = parent
        .map(|parent| repo.find_commit(parent).and_then(|commit| commit.tree()))
        .transpose()
        .map_err(error)?;
    let new = repo.find_tree(tree).map_err(error)?;

    // Diff only .rs files. Treat every file as text, so a file Git guesses
    // is binary still gets its lines in the diff.
    //
    // A renamed file shows up as deleted at its old path and added at its
    // new one, so all its code counts as new. A symlink that became a real
    // file also shows up as deleted plus added. `write_patch` drops the
    // symlink half.
    let mut options = DiffOptions::new();
    options.pathspec("*.rs").force_text(true);
    let diff = repo
        .diff_tree_to_tree(old.as_ref(), Some(&new), Some(&mut options))
        .map_err(error)?;
    let patch = write_patch(&diff).map_err(error)?;

    let mut file = tempfile::NamedTempFile::new().map_err(Error::WriteMutationDiff)?;
    file.write_all(&patch).map_err(Error::WriteMutationDiff)?;
    Ok(Some(file))
}

// Whether the commit's flake.lock has a top-level `mutants-diff` input.
// The input must be declared with `flake = false`: the tool replaces it with
// a single diff file, which is not a flake. Nix's own error for that
// ("flake.nix does not exist") doesn't say what to fix, so report it here.
fn declares_mutants_diff(
    repo: &Repository,
    tree: &git2::Tree<'_>,
    id: &CommitId,
) -> Result<bool, Error> {
    let Some(entry) = tree.get_name("flake.lock") else {
        return Ok(false);
    };
    let blob = repo
        .find_blob(entry.id())
        .map_err(|source| Error::MutationDiff {
            commit: id.clone(),
            source,
        })?;
    let lock: serde_json::Value =
        serde_json::from_slice(blob.content()).map_err(|source| Error::ParseLock {
            commit: id.clone(),
            source,
        })?;
    let root = lock["root"].as_str().unwrap_or("root");
    // The root lists each input with the name of its node in the lock.
    let Some(input) = lock["nodes"][root]["inputs"].get("mutants-diff") else {
        return Ok(false);
    };
    let not_a_flake = input
        .as_str()
        .is_some_and(|node| lock["nodes"][node]["flake"] == false);
    if !not_a_flake {
        return Err(Error::MutantsDiffIsFlake { commit: id.clone() });
    }
    Ok(true)
}

// libgit2 writes the patch, except for its file name lines: cargo-mutants
// can't read how libgit2 writes names with spaces or non-ASCII characters.
fn write_patch(diff: &Diff<'_>) -> Result<Vec<u8>, git2::Error> {
    let mut patch = Vec::new();
    let mut skip = false;
    // Each file in the patch starts with two name lines (`--- a/x.rs` and
    // `+++ b/x.rs`), then its changed lines. Some files have no changed
    // lines: an empty new file, or a file whose permissions alone changed.
    // If such a file's name lines are written, cargo-mutants rejects the
    // whole patch. So hold the name lines here and only write them once
    // the file's first changed lines (a `@@` section) arrive.
    let mut names = Vec::new();
    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let (old, new) = (delta.old_file(), delta.new_file());
        if line.origin_value() == DiffLineType::FileHeader {
            // Skip symlinks and submodules: neither holds Rust code. A
            // symlink's content is the path it points to, and a
            // submodule's is a commit ID. (libgit2's `ignore_submodules`
            // option still shows added and deleted submodules.)
            let skipped = |mode| matches!(mode, FileMode::Link | FileMode::Commit);
            skip = skipped(old.mode()) || skipped(new.mode());
            names.clear();
            if !skip {
                let path = new.path_bytes().unwrap_or_default();
                names.extend(name_line("--- ", "a/", old.exists(), path));
                names.extend(name_line("+++ ", "b/", new.exists(), path));
            }
        } else if !skip {
            if line.origin_value() == DiffLineType::HunkHeader {
                patch.append(&mut names);
            }
            // Hunk headers and "\ No newline at end of file" come complete.
            if matches!(line.origin(), '+' | '-' | ' ') {
                patch.push(line.origin() as u8);
            }
            patch.extend_from_slice(line.content());
        }
        true
    })?;
    Ok(patch)
}

// Write a `---` or `+++` line in a form cargo-mutants can read:
// - A name with `"`, `\`, tab, newline or carriage return goes in quotes,
//   with those written as `\"`, `\\`, `\t`, `\n` and `\r`. cargo-mutants
//   understands no other escapes.
// - Non-ASCII letters stay as they are. Git would write them as octal
//   escapes, which cargo-mutants rejects.
// - A name with a space ends with a tab. Otherwise cargo-mutants cuts the
//   name at the space.
fn name_line(marker: &str, prefix: &str, exists: bool, path: &[u8]) -> Vec<u8> {
    let mut line = marker.as_bytes().to_vec();
    if !exists {
        line.extend(b"/dev/null\n");
        return line;
    }
    let escape = |byte: u8| match byte {
        b'"' | b'\\' => Some(byte),
        b'\t' => Some(b't'),
        b'\n' => Some(b'n'),
        b'\r' => Some(b'r'),
        _ => None,
    };
    let name = [prefix.as_bytes(), path].concat();
    if name.iter().any(|byte| escape(*byte).is_some()) {
        line.push(b'"');
        for byte in name {
            match escape(byte) {
                Some(escaped) => line.extend([b'\\', escaped]),
                None => line.push(byte),
            }
        }
        line.push(b'"');
    } else {
        line.extend(name);
    }
    if path.contains(&b' ') {
        line.push(b'\t');
    }
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::{name_line, prepare_diff};
    use crate::test_support::{Remote, commit_as, empty_tree, tree_with};
    use crate::{CommitId, Error};
    use git2::build::TreeUpdateBuilder;
    use git2::{FileMode, Oid, Repository};

    // A flake.lock that opts in to per-commit diffs.
    const LOCK: &[u8] = br#"{"nodes":{"mutants-diff":{"flake":false},"root":{"inputs":{"mutants-diff":"mutants-diff"}}},"root":"root","version":7}"#;

    fn diff(repo: &mut Repository, id: Oid) -> String {
        let file = prepare_diff(repo, &CommitId(id))
            .unwrap()
            .expect("the flake opts in");
        std::fs::read_to_string(file.path()).unwrap()
    }

    #[test]
    fn diffs_merges_against_their_first_parent() {
        // Main changes a.rs and a side branch changes b.rs. The merge brings
        // in the side branch, so compared with main it changes only b.rs.
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let files: [(&str, &[u8]); 3] = [
            ("flake.lock", LOCK),
            ("src/a.rs", b"fn a() {}\n"),
            ("src/b.rs", b"fn b() {}\n"),
        ];
        let tree = tree_with(&repo, empty_tree(&repo), &files);
        let base = commit_as(&repo, None, "Base", tree, [], 0);
        let main_tree = tree_with(&repo, tree, &[("src/a.rs", b"fn a() { 1; }\n")]);
        let main = commit_as(&repo, None, "Main", main_tree, [base], 1);
        let side_tree = tree_with(&repo, tree, &[("src/b.rs", b"fn b() { 2; }\n")]);
        let side = commit_as(&repo, None, "Side", side_tree, [base], 2);
        let merged = tree_with(&repo, main_tree, &[("src/b.rs", b"fn b() { 2; }\n")]);
        let merge = commit_as(&repo, None, "Merge", merged, [main, side], 3);

        let patch = diff(&mut repo, merge);
        assert!(patch.contains("+++ b/src/b.rs"), "{patch}");
        assert!(!patch.contains("src/a.rs"), "{patch}");
    }

    #[test]
    fn fetches_a_missing_first_parent_in_a_shallow_clone() {
        // In a depth-1 clone the commit's parent is not downloaded, and
        // libgit2 reports no parent at all. The diff must still show only
        // the commit's change, not the whole file as new.
        let remote = Remote::new();
        let repo = &remote.repository;
        let files: [(&str, &[u8]); 2] = [
            ("flake.lock", LOCK),
            ("src/lib.rs", b"fn f() -> i32 {\n    1\n}\n"),
        ];
        let tree = tree_with(repo, empty_tree(repo), &files);
        let parent = commit_as(repo, None, "Parent", tree, [], 0);
        let tree = tree_with(
            repo,
            tree,
            &[("src/lib.rs", b"fn f() -> i32 {\n    10\n}\n")],
        );
        let head = commit_as(repo, None, "Head", tree, [parent], 1);
        let mut clone = remote.clone_shallow(head, 1);
        assert!(!clone.odb().unwrap().exists(parent));

        let patch = diff(&mut clone, head);
        assert!(patch.contains("-    1\n+    10\n"), "{patch}");
    }

    #[test]
    fn writes_file_names_cargo_mutants_reads() {
        // Each odd character in a file name is written so cargo-mutants
        // reads it back correctly. Tabs, newlines and carriage returns are
        // allowed in Linux file names, and Rust can use such files.
        let line = |path: &str| String::from_utf8(name_line("+++ ", "b/", true, path.as_bytes()));
        assert_eq!(line("with space.rs").unwrap(), "+++ b/with space.rs\t\n");
        assert_eq!(
            line(r"back\slash.rs").unwrap(),
            "+++ \"b/back\\\\slash.rs\"\n"
        );
        assert_eq!(line("ünï.rs").unwrap(), "+++ b/ünï.rs\n");
        assert_eq!(
            line(r#"ünï "q" sp\b.rs"#).unwrap(),
            "+++ \"b/ünï \\\"q\\\" sp\\\\b.rs\"\t\n"
        );
        assert_eq!(line("with\ttab.rs").unwrap(), "+++ \"b/with\\ttab.rs\"\n");
        assert_eq!(line("with\nline.rs").unwrap(), "+++ \"b/with\\nline.rs\"\n");
        assert_eq!(
            line("with\rreturn.rs").unwrap(),
            "+++ \"b/with\\rreturn.rs\"\n"
        );
        assert_eq!(
            name_line("--- ", "a/", false, b"gone.rs"),
            b"--- /dev/null\n"
        );
    }

    #[test]
    fn leaves_submodules_out() {
        // The commit removes a submodule at src/old.rs and adds one at
        // src/new.rs. Submodules hold no Rust code, so the diff is empty.
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let base = tree_with(&repo, empty_tree(&repo), &[("flake.lock", LOCK)]);
        let submodule = |tree: Oid, add: &str, remove: Option<&str>| {
            let mut update = TreeUpdateBuilder::new();
            update.upsert(
                add,
                Oid::from_str(&"1".repeat(40)).unwrap(),
                FileMode::Commit,
            );
            if let Some(path) = remove {
                update.remove(path);
            }
            update
                .create_updated(&repo, &repo.find_tree(tree).unwrap())
                .unwrap()
        };
        let tree = submodule(base, "src/old.rs", None);
        let parent = commit_as(&repo, None, "Old", tree, [], 0);
        let tree = submodule(tree, "src/new.rs", Some("src/old.rs"));
        let head = commit_as(&repo, None, "New", tree, [parent], 1);

        assert_eq!(diff(&mut repo, head), "");
    }

    #[test]
    fn leaves_out_files_without_changed_lines() {
        // The commit adds an empty file and changes one line of lib.rs. The
        // empty file has no lines to diff, so it must not appear at all. A
        // file whose permissions alone changed is left out the same way.
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let files: [(&str, &[u8]); 2] = [
            ("flake.lock", LOCK),
            ("src/lib.rs", b"fn f() -> i32 {\n    1\n}\n"),
        ];
        let tree = tree_with(&repo, empty_tree(&repo), &files);
        let parent = commit_as(&repo, None, "Parent", tree, [], 0);
        let tree = tree_with(
            &repo,
            tree,
            &[
                ("src/empty.rs", b""),
                ("src/lib.rs", b"fn f() -> i32 {\n    2\n}\n"),
            ],
        );
        let head = commit_as(&repo, None, "Head", tree, [parent], 1);

        assert_eq!(
            diff(&mut repo, head),
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n fn f() -> i32 {\n-    1\n+    2\n }\n"
        );
    }

    #[test]
    fn treats_a_symlink_replaced_by_a_file_as_an_added_file() {
        // src/lib.rs was a symlink and is now real Rust code. All of that
        // code is new, so it must be in the diff as an added file.
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let base = tree_with(&repo, empty_tree(&repo), &[("flake.lock", LOCK)]);
        let mut update = TreeUpdateBuilder::new();
        update.upsert(
            "src/lib.rs",
            repo.blob(b"other.rs").unwrap(),
            FileMode::Link,
        );
        let tree = update
            .create_updated(&repo, &repo.find_tree(base).unwrap())
            .unwrap();
        let parent = commit_as(&repo, None, "Link", tree, [], 0);
        let tree = tree_with(&repo, tree, &[("src/lib.rs", b"fn f() {}\n")]);
        let head = commit_as(&repo, None, "File", tree, [parent], 1);

        let patch = diff(&mut repo, head);
        assert_eq!(
            patch,
            "--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1 @@\n+fn f() {}\n"
        );
    }
}
