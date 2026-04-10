use std::{fs::File, io::Write, path::Path, process::Command};

use tempfile::TempDir;

use crate::git;

fn exec_git_cmd(args: &str, git_dir: &Path) {
    let res = Command::new("git")
        .arg("-C")
        .arg(git_dir) // execute the git command in this directory
        .args(args.split_whitespace())
        .env_remove("GIT_DIR")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_TERMINAL_PROMPT", "false")
        .env("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_COMMITTER_DATE", "2000-01-02 00:00:00 +0000")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_NAME", "committer")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_1", "main")
        .output()
        .unwrap_or_else(|_| panic!("`git {args}` failed"));
    if !res.status.success() {
        println!("{}", String::from_utf8_lossy(&res.stdout));
        eprintln!("{}", String::from_utf8_lossy(&res.stderr));
        panic!("`git {args}` failed (see output above)")
    }
}

fn create_commit(repo: &Path, add_modified: bool) {
    if add_modified {
        exec_git_cmd("add -A", repo);
    }
    exec_git_cmd("commit -m message", repo);
}

fn empty_git_repo() -> TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir for git testing");
    exec_git_cmd("init", tmp.path());
    exec_git_cmd("config user.email test@helix.org", tmp.path());
    exec_git_cmd("config user.name helix-test", tmp.path());
    tmp
}

#[test]
fn missing_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"foo").unwrap();

    assert!(git::get_diff_base(&file).is_err());
}

#[test]
fn unmodified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    assert_eq!(git::get_diff_base(&file).unwrap(), Vec::from(contents));
}

#[test]
fn modified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    File::create(&file).unwrap().write_all(b"bar").unwrap();

    assert_eq!(git::get_diff_base(&file).unwrap(), Vec::from(contents));
}

/// Test that `get_file_head` does not return content for a directory.
/// This is important to correctly cover cases where a directory is removed and replaced by a file.
/// If the contents of the directory object were returned a diff between a path and the directory children would be produced.
#[test]
fn directory() {
    let temp_git = empty_git_repo();
    let dir = temp_git.path().join("file.txt");
    std::fs::create_dir(&dir).expect("");
    let file = dir.join("file.txt");
    let contents = b"foo".as_slice();
    File::create(file).unwrap().write_all(contents).unwrap();

    create_commit(temp_git.path(), true);

    std::fs::remove_dir_all(&dir).unwrap();
    File::create(&dir).unwrap().write_all(b"bar").unwrap();
    assert!(git::get_diff_base(&dir).is_err());
}

/// Test that `get_diff_base` resolves symlinks so that the same diff base is
/// used as the target file.
///
/// This is important to correctly cover cases where a symlink is removed and
/// replaced by a file. If the contents of the symlink object were returned
/// a diff between a literal file path and the actual file content would be
/// produced (bad ui).
#[cfg(any(unix, windows))]
#[test]
fn symlink() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    let file_link = temp_git.path().join("file_link.txt");

    symlink("file.txt", &file_link).unwrap();
    create_commit(temp_git.path(), true);

    assert_eq!(git::get_diff_base(&file_link).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file).unwrap(), contents);
}

/// Test that `get_diff_base` returns content when the file is a symlink to
/// another file that is in a git repo, but the symlink itself is not.
#[cfg(any(unix, windows))]
#[test]
fn symlink_to_git_repo() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let temp_git = empty_git_repo();

    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    create_commit(temp_git.path(), true);

    let file_link = temp_dir.path().join("file_link.txt");
    symlink(&file, &file_link).unwrap();

    assert_eq!(git::get_diff_base(&file_link).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file).unwrap(), contents);
}

/// Verify `for_each_branch_changed_file` emits both committed changes (between
/// merge-base and HEAD) and working-tree changes, with working-tree winning on
/// overlapping paths.
#[test]
fn branch_changed_files_union() {
    use crate::FileChange;
    use std::collections::BTreeSet;

    let temp_git = empty_git_repo();
    let root = temp_git.path();

    // Initial commit on main with two files.
    File::create(root.join("unchanged.txt"))
        .unwrap()
        .write_all(b"initial")
        .unwrap();
    File::create(root.join("will_be_modified.txt"))
        .unwrap()
        .write_all(b"old")
        .unwrap();
    create_commit(root, true);

    // Create and switch to a feature branch.
    exec_git_cmd("checkout -b feature", root);

    // Commit-level change: new file + modification of existing file.
    File::create(root.join("added_in_branch.txt"))
        .unwrap()
        .write_all(b"new")
        .unwrap();
    File::create(root.join("will_be_modified.txt"))
        .unwrap()
        .write_all(b"committed change")
        .unwrap();
    create_commit(root, true);

    // Working-tree changes: a brand new untracked file, AND an uncommitted
    // modification to a file that was ALSO changed in the branch commit (to
    // verify working-tree wins).
    File::create(root.join("untracked_in_worktree.txt"))
        .unwrap()
        .write_all(b"untracked")
        .unwrap();
    File::create(root.join("will_be_modified.txt"))
        .unwrap()
        .write_all(b"worktree edit on top of commit")
        .unwrap();

    let collected = std::cell::RefCell::new(Vec::<FileChange>::new());
    git::for_each_branch_changed_file(root, |change| {
        if let Ok(c) = change {
            collected.borrow_mut().push(c);
        }
        true
    })
    .unwrap();
    let collected = collected.into_inner();

    let mut display: BTreeSet<String> = BTreeSet::new();
    for change in &collected {
        match change {
            FileChange::Untracked { path } => {
                display.insert(format!(
                    "untracked:{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
            FileChange::Modified { path } => {
                display.insert(format!(
                    "modified:{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
            FileChange::Deleted { path } => {
                display.insert(format!(
                    "deleted:{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
            FileChange::Conflict { path } => {
                display.insert(format!(
                    "conflict:{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
            FileChange::Renamed { from_path, to_path } => {
                display.insert(format!(
                    "renamed:{}->{}",
                    from_path.file_name().unwrap().to_string_lossy(),
                    to_path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }

    // working-tree untracked file appears
    assert!(
        display.contains("untracked:untracked_in_worktree.txt"),
        "untracked working-tree file missing from {display:?}"
    );
    // committed new file appears (as Untracked per our mapping)
    assert!(
        display.contains("untracked:added_in_branch.txt"),
        "branch-committed addition missing from {display:?}"
    );
    // the file that was both committed AND has working-tree edits should only
    // appear once — with the working-tree status (modified), not duplicated.
    let modified_count = display
        .iter()
        .filter(|s| s.ends_with(":will_be_modified.txt"))
        .count();
    assert_eq!(
        modified_count, 1,
        "will_be_modified.txt appeared {modified_count} times in {display:?}; expected 1"
    );
    // unchanged.txt must never appear.
    assert!(
        !display.iter().any(|s| s.ends_with(":unchanged.txt")),
        "unchanged file leaked into branch diff: {display:?}"
    );
}

/// If HEAD is the same as the base branch tip, the branch picker should still
/// show working-tree changes but no committed changes.
#[test]
fn branch_changed_files_head_on_base() {
    use crate::FileChange;

    let temp_git = empty_git_repo();
    let root = temp_git.path();

    File::create(root.join("a.txt"))
        .unwrap()
        .write_all(b"a")
        .unwrap();
    create_commit(root, true);

    // Dirty the working tree without branching.
    File::create(root.join("b.txt"))
        .unwrap()
        .write_all(b"b")
        .unwrap();

    let collected = std::cell::RefCell::new(Vec::<FileChange>::new());
    git::for_each_branch_changed_file(root, |change| {
        if let Ok(c) = change {
            collected.borrow_mut().push(c);
        }
        true
    })
    .unwrap();
    let collected = collected.into_inner();

    assert_eq!(
        collected.len(),
        1,
        "expected exactly one change when HEAD is on base branch"
    );
    assert!(
        matches!(&collected[0], FileChange::Untracked { path } if path.file_name().unwrap() == "b.txt"),
        "expected untracked b.txt"
    );
}
