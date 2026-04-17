use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use gix::filter::plumbing::driver::apply::Delay;
use std::collections::HashSet;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::bstr::ByteSlice;
use gix::diff::Rewrites;
use gix::dir::entry::Status;
use gix::objs::tree::EntryKind;
use gix::sec::trust::DefaultForLevel;
use gix::status::{
    index_worktree::Item,
    plumbing::index_as_worktree::{Change, EntryStatus},
    UntrackedFiles,
};
use gix::{Commit, ObjectId, Repository, ThreadSafeRepository};

use crate::FileChange;

#[cfg(test)]
mod test;

#[inline]
fn get_repo_dir(file: &Path) -> Result<&Path> {
    file.parent().context("file has no parent directory")
}

pub fn get_diff_base(file: &Path) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    // TODO cache repository lookup

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let head = repo.head_commit()?;
    let file_oid = find_file_in_commit(&repo, &head, &file)?;

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    // Get the actual data that git would make out of the git object.
    // This will apply the user's git config or attributes like crlf conversions.
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        worktree_outcome.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(data)
    }
}

/// Returns the content of `file` at the merge-base between HEAD and the base
/// branch (`main` or `master`). Used to power the "branch diff" gutter overlay
/// that shows every line changed in the current branch, including lines that
/// are already committed (not just working-tree edits).
pub fn get_branch_diff_base(file: &Path) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();

    let head_id = repo.head_id()?.detach();

    // Auto-detect base branch: try main, fall back to master.
    let base_ref = repo
        .find_reference("refs/heads/main")
        .or_else(|_| repo.find_reference("refs/heads/master"))
        .context("neither refs/heads/main nor refs/heads/master found")?;
    let base_id = base_ref.into_fully_peeled_id()?.detach();

    let merge_base_id = repo.merge_base(head_id, base_id)?.detach();
    let merge_base_commit = repo.find_commit(merge_base_id)?;

    // If the file didn't exist at the merge-base, `find_file_in_commit` will
    // bail; the caller turns that into `None`, meaning no branch handle is
    // attached (correct — the file is entirely new in the branch).
    let file_oid = find_file_in_commit(&repo, &merge_base_commit, &file)?;

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        worktree_outcome.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(data)
    }
}

pub fn get_current_head_name(file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let head_ref = repo.head_ref()?;
    let head_commit = repo.head_commit()?;

    let name = match head_ref {
        Some(reference) => reference.name().shorten().to_string(),
        None => head_commit.id.to_hex_with_len(8).to_string(),
    };

    Ok(Arc::new(ArcSwap::from_pointee(name.into_boxed_str())))
}

pub fn for_each_changed_file(cwd: &Path, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    status(&open_repo(cwd)?.to_thread_local(), f)
}

pub fn for_each_branch_changed_file(
    cwd: &Path,
    f: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    branch_status(&open_repo(cwd)?.to_thread_local(), f)
}

fn open_repo(path: &Path) -> Result<ThreadSafeRepository> {
    // custom open options
    let mut git_open_opts_map = gix::sec::trust::Mapping::<gix::open::Options>::default();

    // On windows various configuration options are bundled as part of the installations
    // This path depends on the install location of git and therefore requires some overhead to lookup
    // This is basically only used on windows and has some overhead hence it's disabled on other platforms.
    // `gitoxide` doesn't use this as default
    let config = gix::open::permissions::Config {
        system: true,
        git: true,
        user: true,
        env: true,
        includes: true,
        git_binary: cfg!(windows),
    };
    // change options for config permissions without touching anything else
    git_open_opts_map.reduced = git_open_opts_map
        .reduced
        .permissions(gix::open::Permissions {
            config,
            ..gix::open::Permissions::default_for_level(gix::sec::Trust::Reduced)
        });
    git_open_opts_map.full = git_open_opts_map.full.permissions(gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(gix::sec::Trust::Full)
    });

    let open_options = gix::discover::upwards::Options {
        dot_git_only: true,
        ..Default::default()
    };

    let res = ThreadSafeRepository::discover_with_environment_overrides_opts(
        path,
        open_options,
        git_open_opts_map,
    )?;

    Ok(res)
}

/// Emulates the result of running `git status` from the command line.
fn status(repo: &Repository, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let status_platform = repo
        .status(gix::progress::Discard)?
        // Here we discard the `status.showUntrackedFiles` config, as it makes little sense in
        // our case to not list new (untracked) files. We could have respected this config
        // if the default value weren't `Collapsed` though, as this default value would render
        // the feature unusable to many.
        .untracked_files(UntrackedFiles::Files)
        // Turn on file rename detection, which is off by default.
        .index_worktree_rewrites(Some(Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            ..Default::default()
        }));

    // No filtering based on path
    let empty_patterns = vec![];

    let status_iter = status_platform.into_index_worktree_iter(empty_patterns)?;

    for item in status_iter {
        let Ok(item) = item.map_err(|err| f(Err(err.into()))) else {
            continue;
        };
        let change = match item {
            Item::Modification {
                rela_path, status, ..
            } => {
                let path = work_dir.join(rela_path.to_path()?);
                match status {
                    EntryStatus::Conflict { .. } => FileChange::Conflict { path },
                    EntryStatus::Change(Change::Removed) => FileChange::Deleted { path },
                    EntryStatus::Change(Change::Modification { .. }) => {
                        FileChange::Modified { path }
                    }
                    // Files marked with `git add --intent-to-add`. Such files
                    // still show up as new in `git status`, so it's appropriate
                    // to show them the same way as untracked files in the
                    // "changed file" picker. One example of this being used
                    // is Jujutsu, a Git-compatible VCS. It marks all new files
                    // with `--intent-to-add` automatically.
                    EntryStatus::IntentToAdd => FileChange::Untracked { path },
                    _ => continue,
                }
            }
            Item::DirectoryContents { entry, .. } if entry.status == Status::Untracked => {
                FileChange::Untracked {
                    path: work_dir.join(entry.rela_path.to_path()?),
                }
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => FileChange::Renamed {
                from_path: work_dir.join(source.rela_path().to_path()?),
                to_path: work_dir.join(dirwalk_entry.rela_path.to_path()?),
            },
            _ => continue,
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

/// Like [`status`], but shows files that differ between the current branch and
/// its base (`main` or `master`), unioned with working-tree changes. Working-tree
/// changes take precedence when a file appears in both sets.
fn branch_status(repo: &Repository, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let head_id = repo.head_id()?.detach();

    // Auto-detect base branch: try main, fall back to master.
    let base_ref = repo
        .find_reference("refs/heads/main")
        .or_else(|_| repo.find_reference("refs/heads/master"))
        .context("neither refs/heads/main nor refs/heads/master found")?;
    let base_id = base_ref.into_fully_peeled_id()?.detach();

    // Collect working-tree changes first; these win over committed changes.
    let working_tree_changes = collect_status(repo)?;

    // Track which paths the working-tree set already covers so we don't emit duplicates.
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(working_tree_changes.len());
    for change in &working_tree_changes {
        match change {
            FileChange::Untracked { path }
            | FileChange::Modified { path }
            | FileChange::Conflict { path }
            | FileChange::Deleted { path } => {
                seen.insert(path.clone());
            }
            FileChange::Renamed { from_path, to_path } => {
                seen.insert(from_path.clone());
                seen.insert(to_path.clone());
            }
        }
    }

    // Emit working-tree changes immediately.
    for change in working_tree_changes {
        if !f(Ok(change)) {
            return Ok(());
        }
    }

    // Compute committed diff: merge-base(HEAD, base) -> HEAD.
    // Skip if base isn't reachable or we're already at/behind it.
    let merge_base_id = match repo.merge_base(head_id, base_id) {
        Ok(id) => id.detach(),
        Err(err) => {
            // No common ancestor — treat as empty committed diff, but surface
            // the error so the user knows why.
            f(Err(err.into()));
            return Ok(());
        }
    };

    if merge_base_id == head_id {
        return Ok(());
    }

    let merge_base_tree = repo.find_commit(merge_base_id)?.tree()?;
    let head_tree = repo.find_commit(head_id)?.tree()?;

    let mut platform = merge_base_tree.changes()?;
    // Disable rewrite tracking for speed; we don't need rename detection here.
    // Addition + Deletion for a rename is still useful output.
    platform.options(|opts| {
        opts.track_path();
        opts.track_rewrites(None);
    });

    let mut committed: Vec<FileChange> = Vec::new();
    platform.for_each_to_obtain_tree(
        &head_tree,
        |change| -> std::result::Result<ControlFlow<()>, std::convert::Infallible> {
            use gix::object::tree::diff::Change as DiffChange;
            let file_change = match change {
                DiffChange::Addition {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !matches!(
                        entry_mode.kind(),
                        EntryKind::Blob | EntryKind::BlobExecutable
                    ) {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let Ok(rel) = location.to_path() else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    FileChange::Untracked {
                        path: work_dir.join(rel),
                    }
                }
                DiffChange::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !matches!(
                        entry_mode.kind(),
                        EntryKind::Blob | EntryKind::BlobExecutable
                    ) {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let Ok(rel) = location.to_path() else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    FileChange::Deleted {
                        path: work_dir.join(rel),
                    }
                }
                DiffChange::Modification {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !matches!(
                        entry_mode.kind(),
                        EntryKind::Blob | EntryKind::BlobExecutable
                    ) {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let Ok(rel) = location.to_path() else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    FileChange::Modified {
                        path: work_dir.join(rel),
                    }
                }
                DiffChange::Rewrite {
                    source_location,
                    location,
                    ..
                } => {
                    let (Ok(from), Ok(to)) = (source_location.to_path(), location.to_path()) else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    FileChange::Renamed {
                        from_path: work_dir.join(from),
                        to_path: work_dir.join(to),
                    }
                }
            };
            committed.push(file_change);
            Ok(ControlFlow::Continue(()))
        },
    )?;

    for change in committed {
        let already_seen = match &change {
            FileChange::Untracked { path }
            | FileChange::Modified { path }
            | FileChange::Conflict { path }
            | FileChange::Deleted { path } => seen.contains(path),
            FileChange::Renamed { from_path, to_path } => {
                seen.contains(from_path) || seen.contains(to_path)
            }
        };
        if already_seen {
            continue;
        }
        if !f(Ok(change)) {
            return Ok(());
        }
    }

    Ok(())
}

/// Run working-tree status and collect the results into a `Vec` so a caller
/// can inspect them before emitting. Mirrors [`status`] internally.
fn collect_status(repo: &Repository) -> Result<Vec<FileChange>> {
    let out = std::cell::RefCell::new(Vec::new());
    status(repo, |change| {
        if let Ok(c) = change {
            out.borrow_mut().push(c);
        }
        true
    })?;
    Ok(out.into_inner())
}

/// A single-line `git blame` result: who last touched the line and in which
/// commit. Everything is owned so the caller can move it across threads and
/// render it after the repo handle is dropped.
#[derive(Clone, Debug)]
pub struct BlameLine {
    /// Short hex commit id (e.g. `abc1234`).
    pub commit_id: String,
    pub author: String,
    pub author_email: String,
    /// Author time as unix seconds, UTC.
    pub time_seconds: i64,
    /// First line of the commit message.
    pub summary: String,
}

/// Blame a single `line` (0-based, HEAD-side line number) of `file` and return
/// the commit that last modified it.
///
/// The caller is responsible for translating buffer lines into HEAD-side line
/// numbers when the working tree differs from HEAD (e.g. by consulting the
/// [`crate::DiffHandle`]); a line that exists only in the working tree has no
/// blame and this function will not be called for it.
pub fn blame_line(file: &Path, line: u32) -> Result<BlameLine> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();

    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("repo has no worktree"))?;
    let rela = file
        .strip_prefix(work_dir)
        .context("file is outside the worktree")?;
    let rela_bstr = gix::path::try_into_bstr(rela)?;

    let head_id = repo.head_id()?.detach();
    let outcome = repo
        .blame_file(rela_bstr.as_ref(), head_id, Default::default())
        .context("gix blame failed")?;

    let entry = outcome
        .entries
        .iter()
        .find(|e| {
            let end = e.start_in_blamed_file + e.len.get();
            (e.start_in_blamed_file..end).contains(&line)
        })
        .ok_or_else(|| anyhow::anyhow!("line {line} has no blame entry"))?;

    let commit = repo.find_commit(entry.commit_id)?;
    let short_id = commit.short_id()?.to_string();
    let commit_ref = commit.decode()?;
    let sig = commit_ref.author()?;
    let summary = commit_ref
        .message()
        .summary()
        .to_str_lossy()
        .into_owned();

    Ok(BlameLine {
        commit_id: short_id,
        author: sig.name.to_str_lossy().into_owned(),
        author_email: sig.email.to_str_lossy().into_owned(),
        time_seconds: sig.time()?.seconds,
        summary,
    })
}

/// Finds the object that contains the contents of a file at a specific commit.
fn find_file_in_commit(repo: &Repository, commit: &Commit, file: &Path) -> Result<ObjectId> {
    let repo_dir = repo.workdir().context("repo has no worktree")?;
    let rel_path = file.strip_prefix(repo_dir)?;
    let tree = commit.tree()?;
    let tree_entry = tree
        .lookup_entry_by_path(rel_path)?
        .context("file is untracked")?;
    match tree_entry.mode().kind() {
        // not a file, everything is new, do not show diff
        mode @ (EntryKind::Tree | EntryKind::Commit | EntryKind::Link) => {
            bail!("entry at {} is not a file but a {mode:?}", file.display())
        }
        // found a file
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(tree_entry.object_id()),
    }
}
