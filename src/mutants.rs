// Per-commit mutation testing: give each commit's flake the diff of that
// commit, so `cargo mutants --in-diff` only mutates code the commit changed.

use gix::bstr::ByteSlice;
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use gix::object::tree::diff::ChangeDetached;
use gix::{ObjectId, Repository};
use std::io::Write;

use crate::shallow::fetch;
use crate::{CommitId, Error, Source};

// Build the commit's diff against its first parent, if its flake opts in by
// declaring a top-level `mutants-diff` input in its committed flake.lock.
// The diff is written to a temporary file outside the checkout.
pub(crate) fn prepare_diff(
    repo: &Repository,
    id: &CommitId,
) -> Result<Option<tempfile::NamedTempFile>, Error> {
    let error = diff_error(id);
    let commit = repo.find_commit(id.0).map_err(&error)?;
    let tree = commit.tree().map_err(&error)?;
    if !declares_mutants_diff(&tree, id)? {
        return Ok(None);
    }

    // Merges use their first parent, and a root commit the empty tree. gix
    // reports the first parent even past a shallow boundary. Fetch it there
    // rather than diffing against nothing; in a full clone a missing parent
    // means a broken repository, so let the lookup below report it.
    let parent = commit.parent_ids().next().map(|parent| parent.detach());
    let parent_tree = match parent {
        Some(parent) => {
            if !repo.has_object(parent) && repo.is_shallow().map_err(&error)? {
                fetch(repo, &[parent], None, &mut std::io::sink())?;
            }
            let parent = repo.find_commit(parent).map_err(&error)?;
            Some(parent.tree().map_err(&error)?)
        }
        None => None,
    };

    let mut options = gix::diff::Options::default().with_rewrites(None);
    options.track_path();
    let changes = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), options)
        .map_err(&error)?;
    let mut patch = Vec::new();
    for change in changes {
        let (path, before, after) = match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } => (location, None, Some((entry_mode, id))),
            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => (location, Some((entry_mode, id)), None),
            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
                ..
            } => (
                location,
                Some((previous_entry_mode, previous_id)),
                Some((entry_mode, id)),
            ),
            // Rename detection is off, so there are no rewrites.
            ChangeDetached::Rewrite { .. } => continue,
        };
        // cargo-mutants only reads Rust source files. A symlink or submodule
        // side counts as absent, so a symlink replaced by a regular file is
        // an addition and the reverse a deletion.
        let is_file = |side: &(gix::object::tree::EntryMode, ObjectId)| side.0.is_blob();
        let (before, after) = (before.filter(is_file), after.filter(is_file));
        if !path.ends_with(b".rs") || (before.is_none() && after.is_none()) {
            continue;
        }
        let read = |side: Option<(gix::object::tree::EntryMode, ObjectId)>| match side {
            Some((_, id)) => Ok(repo.find_blob(id).map_err(&error)?.take_data()),
            None => Ok(Vec::new()),
        };
        let (old, new) = (read(before)?, read(after)?);
        write_file_patch(
            &mut patch,
            &path,
            before.is_some(),
            after.is_some(),
            &old,
            &new,
        )
        .map_err(Error::WriteMutationDiff)?;
    }

    let mut file = tempfile::NamedTempFile::new().map_err(Error::WriteMutationDiff)?;
    file.write_all(&patch).map_err(Error::WriteMutationDiff)?;
    Ok(Some(file))
}

// Wrap any gix error as a failure to build this commit's diff.
fn diff_error<E: Into<Source>>(id: &CommitId) -> impl Fn(E) -> Error {
    move |source| Error::MutationDiff {
        commit: id.clone(),
        source: source.into(),
    }
}

// Whether the commit's flake.lock has a top-level `mutants-diff` input.
fn declares_mutants_diff(tree: &gix::Tree<'_>, id: &CommitId) -> Result<bool, Error> {
    let Some(entry) = tree.find_entry("flake.lock") else {
        return Ok(false);
    };
    let lock = entry.object().map_err(diff_error(id))?;
    let lock: serde_json::Value =
        serde_json::from_slice(&lock.data).map_err(|source| Error::ParseLock {
            commit: id.clone(),
            source,
        })?;
    let root = lock["root"].as_str().unwrap_or("root");
    Ok(lock["nodes"][root]["inputs"].get("mutants-diff").is_some())
}

// Write one file's changes in Git's patch format, which cargo-mutants reads.
fn write_file_patch(
    patch: &mut Vec<u8>,
    path: &[u8],
    existed: bool,
    exists: bool,
    old: &[u8],
    new: &[u8],
) -> std::io::Result<()> {
    let (a, b) = (header_path("a/", path), header_path("b/", path));
    patch.extend(b"diff --git ");
    patch.extend(&a);
    patch.push(b' ');
    patch.extend(&b);
    patch.push(b'\n');
    // Like Git, end a name that has spaces with a tab, so readers know
    // where it stops.
    let end: &[u8] = if a[0] != b'"' && path.contains(&b' ') {
        b"\t\n"
    } else {
        b"\n"
    };
    for (marker, name, present) in [(b"--- ", &a, existed), (b"+++ ", &b, exists)] {
        patch.extend(marker);
        if present {
            patch.extend(name);
            patch.extend(end);
        } else {
            patch.extend(b"/dev/null\n");
        }
    }
    let input = InternedInput::new(old, new);
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    let hunks = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(Vec::new(), "\n"),
        ContextSize::symmetrical(3),
    )
    .consume()?;
    patch.extend_from_slice(&hunks);
    Ok(())
}

// A path as Git writes it in patch headers. gix quotes it like Git, but
// also turns non-ASCII into octal escapes, which cargo-mutants can't read.
// So a name that is only non-ASCII stays as is, like Git's output with
// `core.quotePath=false`.
fn header_path(prefix: &str, path: &[u8]) -> Vec<u8> {
    let full = [prefix.as_bytes(), path].concat();
    let special = |byte: &u8| *byte < 0x20 || *byte == 0x7f || matches!(byte, b'"' | b'\\');
    if !full.iter().any(special) {
        return full;
    }
    gix::quote::ansi_c::quote(full.as_bstr()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::{prepare_diff, write_file_patch};
    use crate::CommitId;
    use crate::test_support::{Remote, commit_as, tree_with, with_test_identity};
    use gix::{ObjectId, Repository};

    // A flake.lock that opts in to per-commit diffs.
    const LOCK: &[u8] =
        br#"{"nodes":{"root":{"inputs":{"mutants-diff":"mutants-diff"}}},"root":"root","version":7}"#;

    fn diff(repo: &Repository, id: ObjectId) -> String {
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
        let repo = with_test_identity(gix::init(root.path()).unwrap());
        let files: [(&str, &[u8]); 3] = [
            ("flake.lock", LOCK),
            ("src/a.rs", b"fn a() {}\n"),
            ("src/b.rs", b"fn b() {}\n"),
        ];
        let tree = tree_with(&repo, repo.empty_tree().id, &files);
        let base = commit_as(&repo, None, "Base", tree, [], 0);
        let main_tree = tree_with(&repo, tree, &[("src/a.rs", b"fn a() { 1; }\n")]);
        let main = commit_as(&repo, None, "Main", main_tree, [base], 1);
        let side_tree = tree_with(&repo, tree, &[("src/b.rs", b"fn b() { 2; }\n")]);
        let side = commit_as(&repo, None, "Side", side_tree, [base], 2);
        let merged = tree_with(&repo, main_tree, &[("src/b.rs", b"fn b() { 2; }\n")]);
        let merge = commit_as(&repo, None, "Merge", merged, [main, side], 3);

        let patch = diff(&repo, merge);
        assert!(patch.contains("+++ b/src/b.rs"), "{patch}");
        assert!(!patch.contains("src/a.rs"), "{patch}");
    }

    #[test]
    fn fetches_a_missing_first_parent_in_a_shallow_clone() {
        // In a depth-1 clone the commit's parent is not downloaded. The diff
        // must still show only the commit's change, not the whole file as new.
        let remote = Remote::new();
        let repo = &remote.repository;
        let files: [(&str, &[u8]); 2] = [
            ("flake.lock", LOCK),
            ("src/lib.rs", b"fn f() -> i32 {\n    1\n}\n"),
        ];
        let tree = tree_with(repo, repo.empty_tree().id, &files);
        let parent = commit_as(repo, None, "Parent", tree, [], 0);
        let tree = tree_with(
            repo,
            tree,
            &[("src/lib.rs", b"fn f() -> i32 {\n    10\n}\n")],
        );
        let head = commit_as(repo, None, "Head", tree, [parent], 1);
        let clone = remote.clone_shallow(head, 1);
        assert!(!clone.has_object(parent));

        let patch = diff(&clone, head);
        assert!(patch.contains("-    1\n+    10\n"), "{patch}");
    }

    #[test]
    fn writes_unusual_paths_the_way_git_does() {
        // cargo-mutants reads paths as Git writes them with
        // core.quotePath=false: a tab after a name with spaces, C-style
        // quotes around a name with backslashes, and non-ASCII left as is.
        // These are Git's own headers.
        let headers = |path: &[u8]| {
            let mut patch = Vec::new();
            write_file_patch(&mut patch, path, true, true, b"a\n", b"b\n").unwrap();
            let patch = String::from_utf8(patch).unwrap();
            patch.lines().take(3).collect::<Vec<_>>().join("\n")
        };
        assert_eq!(
            headers(b"with space.rs"),
            "diff --git a/with space.rs b/with space.rs\n\
             --- a/with space.rs\t\n\
             +++ b/with space.rs\t"
        );
        assert_eq!(
            headers(br"back\slash.rs"),
            r#"diff --git "a/back\\slash.rs" "b/back\\slash.rs"
--- "a/back\\slash.rs"
+++ "b/back\\slash.rs""#
        );
        assert_eq!(
            headers("ünï.rs".as_bytes()),
            "diff --git a/ünï.rs b/ünï.rs\n--- a/ünï.rs\n+++ b/ünï.rs"
        );
    }

    #[test]
    fn treats_a_symlink_replaced_by_a_file_as_an_added_file() {
        // src/lib.rs was a symlink and is now real Rust code. All of that
        // code is new, so it must be in the diff as an added file.
        let root = tempfile::tempdir().unwrap();
        let repo = with_test_identity(gix::init(root.path()).unwrap());
        let link = repo.write_blob(b"other.rs").unwrap();
        let mut tree = repo
            .edit_tree(tree_with(
                &repo,
                repo.empty_tree().id,
                &[("flake.lock", LOCK)],
            ))
            .unwrap();
        tree.upsert("src/lib.rs", gix::object::tree::EntryKind::Link, link)
            .unwrap();
        let tree = tree.write().unwrap().detach();
        let parent = commit_as(&repo, None, "Link", tree, [], 0);
        let tree = tree_with(&repo, tree, &[("src/lib.rs", b"fn f() {}\n")]);
        let head = commit_as(&repo, None, "File", tree, [parent], 1);

        let patch = diff(&repo, head);
        assert!(
            patch.contains("--- /dev/null\n+++ b/src/lib.rs\n"),
            "{patch}"
        );
        assert!(patch.contains("+fn f() {}"), "{patch}");
    }
}
