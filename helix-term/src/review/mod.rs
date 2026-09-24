//! Review controller: context epochs reject delayed results before touching the UI.
//! Local HEAD is checked before rendering; remote work is asynchronous and explicit.
mod commits;
mod github;
pub(crate) mod reply;

use crate::{
    compositor, job,
    ui::{
        overlay::overlaid,
        picker::{FileLocation, PathOrId},
        Picker, PickerColumn,
    },
};
use helix_core::{Rope, Selection, Transaction};
use helix_view::{
    editor::Action,
    review::{self, safe_text, Block, Context, DocumentReview},
    Editor,
};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

fn read_small(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= 4 * 1024 * 1024).then_some(bytes)
}

// Resolve symlinks first, then use the editor's platform path spelling (notably
// Windows verbatim-prefix simplification). normalize alone is not containment.
fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(helix_stdx::path::normalize)
}
fn relative_key(path: &Path) -> Option<String> {
    path.components()
        .map(|c| match c {
            std::path::Component::Normal(c) => c.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

/// Resolve worktree gitdirs and common refs without spawning a process in the UI.
/// Do not cache HEAD: a late response after a checkout must never paint old data.
fn context_at(path: &Path) -> Option<Context> {
    let root = canonical(path.ancestors().find(|dir| dir.join(".git").exists())?).ok()?;
    let git = root.join(".git");
    let git_dir = if git.is_dir() {
        git
    } else {
        let text = String::from_utf8(read_small(&git)?).ok()?;
        canonical(&root.join(text.trim().strip_prefix("gitdir: ")?)).ok()?
    };
    let common = read_small(&git_dir.join("commondir"))
        .and_then(|s| String::from_utf8(s).ok())
        .map(|s| git_dir.join(s.trim()))
        .unwrap_or_else(|| git_dir.clone());
    let head = String::from_utf8(read_small(&git_dir.join("HEAD"))?).ok()?;
    let reference = head.trim().strip_prefix("ref: ")?;
    let branch = reference.strip_prefix("refs/heads/")?.to_owned();
    if !github::valid_path(reference) {
        return None;
    }
    let oid = read_small(&common.join(reference))
        .and_then(|s| String::from_utf8(s).ok())
        .or_else(|| {
            let refs = String::from_utf8(read_small(&common.join("packed-refs"))?).ok()?;
            refs.lines().find_map(|line| {
                line.split_once(' ')
                    .filter(|(_, name)| *name == reference)
                    .map(|(oid, _)| oid.to_owned())
            })
        })?;
    let mut config = read_small(&common.join("config")).unwrap_or_default();
    config.extend(read_small(&git_dir.join("config.worktree")).unwrap_or_default());
    Some(Context {
        root,
        git_dir,
        branch,
        head: oid.trim().to_owned(),
        config,
    })
}

/// Explain why `context_at` found no reviewable branch at `path`.
fn missing_context(path: &Path) -> String {
    let Some(root) = path.ancestors().find(|dir| dir.join(".git").exists()) else {
        return format!(
            "Reviews: {} is not inside a Git repository",
            helix_stdx::path::fold_home_dir(path).display()
        );
    };
    let git = root.join(".git");
    let head = if git.is_dir() {
        read_small(&git.join("HEAD"))
    } else {
        read_small(&git)
            .and_then(|s| String::from_utf8(s).ok())
            .and_then(|s| Some(root.join(s.trim().strip_prefix("gitdir: ")?).join("HEAD")))
            .and_then(|head| read_small(&head))
    }
    .and_then(|s| String::from_utf8(s).ok());
    match head.as_deref().map(str::trim) {
        Some(head) if head.starts_with("ref: refs/heads/") => format!(
            "Reviews: cannot read branch {} (no commits yet or unsupported ref storage)",
            safe_text(head.trim_start_matches("ref: refs/heads/"))
        ),
        Some(head) if !head.starts_with("ref: ") => {
            "Reviews: detached HEAD; check out the PR branch to show its discussions".into()
        }
        _ => format!(
            "Reviews: no readable Git HEAD in {}",
            helix_stdx::path::fold_home_dir(&git).display()
        ),
    }
}

fn current_path(editor: &Editor) -> PathBuf {
    doc!(editor)
        .path()
        .and_then(|p| p.parent())
        .map(Path::to_owned)
        .or_else(|| editor.review.context.as_ref().map(|c| c.root.clone()))
        .unwrap_or_else(helix_stdx::env::current_working_dir)
}

fn current_context(editor: &Editor) -> Option<Context> {
    context_at(&current_path(editor))
}

fn clear_documents(editor: &mut Editor) {
    for doc in editor.documents_mut() {
        doc.review = DocumentReview::default();
    }
}

/// Called at rendering boundaries and before review commands. No network/process
/// wait can block editing. An epoch changes for refresh, repository and branch changes.
pub(crate) fn synchronize(editor: &mut Editor) {
    if !editor.review.enabled || editor.should_close() {
        return;
    }
    let context = current_context(editor);
    if context != editor.review.context {
        editor.review.invalidate(context.clone());
        clear_documents(editor);
        editor.review.status.clear();
    }
    let Some(context) = context else {
        editor.review.status = missing_context(&current_path(editor));
        editor.review.failed = true;
        return;
    };
    if editor.review.review.is_none() && !editor.review.loading && editor.review.status.is_empty() {
        editor.review.loading = true;
        editor.review.status = "Loading GitHub reviews…".into();
        let epoch = editor.review.generation;
        let selection = editor.review.pull.clone();
        let task = tokio::spawn(async move {
            let result = github::fetch(&context, selection).await;
            job::dispatch(move |editor, _| {
                // Re-read local context here as well: callback delivery can precede render.
                if !editor.review.accepts(epoch, &context)
                    || current_context(editor).as_ref() != Some(&context)
                {
                    return;
                }
                editor.review.loading = false;
                editor.review.task = None;
                match result {
                    Ok(Some(review)) => {
                        editor.review.status = review.summary();
                        editor.review.review = Some(Arc::new(review));
                        editor.set_status(editor.review.status.clone());
                    }
                    Ok(None) => {
                        editor.review.status = format!(
                            "Reviews: no open pull request for branch {}; use :review-select owner/repo#number",
                            safe_text(&context.branch)
                        );
                        editor.set_status(editor.review.status.clone());
                    }
                    Err(err) => {
                        editor.review.failed = true;
                        editor.review.status = safe_text(&format!("Reviews: {err}"));
                        editor.set_error(editor.review.status.clone());
                    }
                }
            })
            .await;
        });
        editor.review.task = Some(task.abort_handle());
    }
    attach_documents(editor);
}

fn attach_documents(editor: &mut Editor) {
    let Some(context) = editor.review.context.clone() else {
        return;
    };
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    let generation = editor.review.generation;
    let expanded = editor.review.expanded.clone();
    for doc in editor.documents_mut() {
        let Some(path) = doc.path().cloned() else {
            continue;
        };
        let stamp = (generation, doc.version(), path.clone());
        if doc.review.stamp.as_ref() == Some(&stamp) {
            continue;
        }
        doc.review = DocumentReview {
            stamp: Some(stamp),
            blocks: Vec::new(),
            task: None,
        };
        // Resolve symlinks before comparing paths. Never bind an outside file via
        // a symlink, or a nested repository's buffer to the outer repository.
        // The buffer may be opened through a symlinked directory (for example a
        // symlinked checkout); only its resolved location decides membership.
        let Ok(real) = canonical(&path) else {
            continue;
        };
        if context_at(real.parent().unwrap_or(&real))
            .as_ref()
            .map(|c| &c.root)
            != Some(&context.root)
        {
            continue;
        }
        let Ok(relative) = real.strip_prefix(&context.root) else {
            continue;
        };
        let Some(relative) = relative_key(relative).filter(|p| review.sources.contains_key(p))
        else {
            continue;
        };
        let doc_id = doc.id();
        let version = doc.version();
        let text = doc.text().clone();
        let context = context.clone();
        let review = review.clone();
        let expanded = expanded.clone();
        // Coalesce typing before doing CPU work. Dropping DocumentReview aborts
        // the debounce and callback; submitted mappers finish on their snapshots.
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            let Ok(blocks) = tokio::task::spawn_blocking(move || {
                map_blocks(&review, &relative, &text, &expanded)
            })
            .await
            else {
                return;
            };
            job::dispatch(move |editor, _| {
                if !editor.review.accepts(generation, &context)
                    || current_context(editor).as_ref() != Some(&context)
                    || !canonical(&path).is_ok_and(|p| p == real)
                {
                    return;
                }
                let Some(doc) = editor.documents.get_mut(&doc_id) else {
                    return;
                };
                if doc.version() != version
                    || doc.path() != Some(&path)
                    || doc.review.stamp.as_ref() != Some(&(generation, version, path.clone()))
                {
                    return;
                }
                doc.review.blocks = blocks;
                doc.review.task = None;
                if let Some(pending) = editor.review.pending_selection {
                    if pending.document == doc_id {
                        editor.review.pending_selection = None;
                    }
                    let (view, doc) = current_ref!(editor);
                    if pending.document == doc_id
                        && view.id == pending.view
                        && doc.version() == pending.version
                        && doc.id() == pending.document
                        && doc
                            .selection(view.id)
                            .primary()
                            .cursor(doc.text().slice(..))
                            == pending.cursor
                    {
                        finish_selection(editor, pending.index);
                        return;
                    }
                }
                if view!(editor).doc == doc_id {
                    editor.ensure_cursor_in_view(view!(editor).id);
                }
            })
            .await;
        });
        doc.review.task = Some(task.abort_handle());
    }
}

fn map_blocks(
    review: &review::Review,
    relative: &str,
    text: &Rope,
    expanded: &std::collections::HashSet<String>,
) -> Vec<Block> {
    let Some(source) = review.sources.get(relative) else {
        return Vec::new();
    };
    let Some(mapper) = review::LineMapper::new(source, text) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for thread in &review.threads {
        if !github::valid_path(&thread.path) || relative != thread.path {
            continue;
        }
        let Some(lines) = thread.lines.clone() else {
            continue;
        };
        let Some(range) = mapper.map(lines) else {
            continue;
        };
        let mut anchor = range.end.saturating_sub(1);
        if anchor > 0 && text.char(anchor) == '\n' && text.char(anchor - 1) == '\r' {
            anchor -= 1;
        }
        if range.end == text.len_chars() && text.char(anchor) != '\n' && text.char(anchor) != '\r' {
            anchor = range.end;
        }
        blocks.push(Block {
            thread: thread.clone(),
            range,
            anchor,
            expanded: expanded.contains(&thread.id),
        });
    }
    blocks.sort_by_key(|b| b.anchor);
    blocks
}

pub(crate) fn toggle(cx: &mut compositor::Context) {
    cx.editor.review.enabled = !cx.editor.review.enabled;
    cx.editor.review.invalidate(None);
    cx.editor.review.status.clear();
    clear_documents(cx.editor);
    synchronize(cx.editor);
    if !cx.editor.review.enabled {
        cx.editor.set_status("GitHub review comments hidden");
    } else if cx.editor.review.context.is_none() {
        // Never hide why nothing will be shown behind a generic confirmation.
        cx.editor.set_error(cx.editor.review.status.clone());
    } else {
        cx.editor.set_status(format!(
            "GitHub review comments enabled · {}",
            cx.editor.review.status
        ));
    }
}
pub(crate) fn refresh(cx: &mut compositor::Context, selection: Option<String>) {
    refresh_editor(cx.editor, selection);
    if cx.editor.review.context.is_none() {
        cx.editor.set_error(cx.editor.review.status.clone());
    } else {
        cx.editor.set_status(cx.editor.review.status.clone());
    }
}

fn refresh_editor(editor: &mut Editor, selection: Option<String>) {
    editor.review.enabled = true;
    let context = current_context(editor);
    let selection = selection.or_else(|| {
        (context == editor.review.context)
            .then(|| editor.review.pull.clone())
            .flatten()
    });
    editor.review.invalidate(context);
    editor.review.pull = selection;
    editor.review.status.clear();
    clear_documents(editor);
    synchronize(editor);
}

fn under_cursor(editor: &Editor) -> Option<usize> {
    let review = editor.review.review.as_ref()?;
    let (view, doc) = current_ref!(editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor(doc.text().slice(..));
    let line = doc.text().char_to_line(cursor);
    let candidates: Vec<_> = doc
        .review
        .blocks
        .iter()
        .filter(|b| b.range.contains(&cursor) || doc.text().char_to_line(b.anchor) == line)
        .filter_map(|b| review.threads.iter().position(|t| t.id == b.thread.id))
        .collect();
    editor
        .review
        .selected
        .filter(|i| candidates.contains(i))
        .or_else(|| candidates.first().copied())
        .or(editor.review.selected)
}

pub(crate) fn expand(cx: &mut compositor::Context) {
    synchronize(cx.editor);
    let Some(index) = under_cursor(cx.editor) else {
        cx.editor
            .set_error("No review discussion selected; use :review-next or :review-list");
        return;
    };
    let id = cx.editor.review.review.as_ref().unwrap().threads[index]
        .id
        .clone();
    if !cx.editor.review.expanded.remove(&id) {
        cx.editor.review.expanded.insert(id);
    }
    // Reject an older mapping result without invalidating fetched data.
    cx.editor.review.generation = cx.editor.review.generation.wrapping_add(1);
    for doc in cx.editor.documents_mut() {
        doc.review.stamp = None;
    }
    attach_documents(cx.editor);
}

pub(crate) fn navigate(cx: &mut compositor::Context, previous: bool) {
    synchronize(cx.editor);
    let Some(review) = cx
        .editor
        .review
        .review
        .clone()
        .filter(|r| !r.threads.is_empty())
    else {
        cx.editor.set_status(cx.editor.review.status.clone());
        return;
    };
    let count = review.threads.len();
    let index = match under_cursor(cx.editor) {
        Some(i) if previous => (i + count - 1) % count,
        Some(i) => (i + 1) % count,
        None if previous => count - 1,
        None => 0,
    };
    select(cx.editor, index);
}

fn select(editor: &mut Editor, index: usize) {
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    let Some(thread) = review.threads.get(index) else {
        return;
    };
    editor.review.selected = Some(index);
    editor.review.pending_selection = None;
    let Some(context) = editor.review.context.as_ref() else {
        return;
    };
    if github::valid_path(&thread.path) {
        let path = context.root.join(&thread.path);
        // Remote paths are never opened without canonical containment checks.
        if canonical(&path).is_ok_and(|p| p.starts_with(&context.root) && p == path)
            && path.is_file()
            && open_document(editor, &path)
        {
            attach_documents(editor);
            let (view, doc) = current_ref!(editor);
            if doc.review.task.is_some() {
                editor.review.pending_selection = Some(review::PendingSelection {
                    index,
                    document: doc.id(),
                    view: view.id,
                    cursor: doc
                        .selection(view.id)
                        .primary()
                        .cursor(doc.text().slice(..)),
                    version: doc.version(),
                });
                editor.set_status("Mapping review location…");
            } else {
                finish_selection(editor, index);
            }
            return;
        }
    }
    open_thread(editor, index);
}

/// Open `path` (already canonical), reusing a buffer that was opened through a
/// symlinked directory instead of creating a duplicate of the same file.
fn open_document(editor: &mut Editor, path: &Path) -> bool {
    let existing = editor
        .documents()
        .find(|doc| {
            doc.path()
                .is_some_and(|p| canonical(p).is_ok_and(|p| p == path))
        })
        .map(|doc| doc.id());
    match existing {
        Some(id) => {
            editor.switch(id, Action::Replace);
            true
        }
        None => editor.open(path, Action::Replace).is_ok(),
    }
}

fn finish_selection(editor: &mut Editor, index: usize) {
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    let Some(thread) = review.threads.get(index) else {
        return;
    };
    let (view, doc) = current!(editor);
    if let Some(block) = doc.review.blocks.iter().find(|b| b.thread.id == thread.id) {
        doc.set_selection(
            view.id,
            Selection::point(block.anchor.min(doc.text().len_chars())),
        );
        helix_view::align_view(doc, view, helix_view::Align::Top);
        editor.set_status(format!(
            "Review {}/{} · {} · :review-expand / :review-open",
            index + 1,
            review.threads.len(),
            thread.title()
        ));
    } else {
        open_thread(editor, index);
    }
}

fn open_thread(editor: &mut Editor, index: usize) {
    editor.review.pending_selection = None;
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    let Some(thread) = review.threads.get(index) else {
        return;
    };
    let mapped = editor
        .documents
        .values()
        .any(|d| d.review.blocks.iter().any(|b| b.thread.id == thread.id));
    let text = format!(
        "{}\n{}\n{}\n{}\n\n{}\n\nOriginal diff context:\n{}\n\n{}\n",
        review.label,
        thread.location,
        if mapped {
            "Location mapped to current document"
        } else {
            "Location unavailable in current document; original context only"
        },
        thread.title(),
        thread.conversation(),
        thread.diff,
        thread.url
    );
    editor.review.selected = Some(index);
    let id = if let Some(id) = editor.review.document.filter(|id| {
        editor
            .documents
            .get(id)
            .is_some_and(|doc| doc.path().is_none() && !doc.is_modified())
    }) {
        editor.switch(id, Action::Replace);
        id
    } else {
        let id = editor.new_file(Action::Replace);
        editor.review.document = Some(id);
        id
    };
    let (view, doc) = current!(editor);
    debug_assert_eq!(doc.id(), id);
    doc.set_virtual_name(Some("[github-review]".into()));
    doc.set_soft_wrap_override(Some(true));
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(text.into()))].into_iter(),
    )
    .with_selection(Selection::point(0));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    doc.readonly = true;
}

pub(crate) fn open(cx: &mut compositor::Context) {
    synchronize(cx.editor);
    if let Some(index) = under_cursor(cx.editor) {
        open_thread(cx.editor, index);
    } else {
        cx.editor
            .set_error("No discussion selected; use :review-next or :review-list");
    }
}

struct ListEntry {
    index: usize,
    thread: Arc<review::Thread>,
    /// Local file for previews, only when safely inside the repository.
    path: Option<PathBuf>,
}

fn list_entries(review: &review::Review, root: Option<&Path>) -> Vec<ListEntry> {
    review
        .threads
        .iter()
        .enumerate()
        .map(|(index, thread)| ListEntry {
            index,
            thread: thread.clone(),
            path: root
                .filter(|_| github::valid_path(&thread.path))
                .map(|root| root.join(&thread.path))
                .filter(|path| canonical(path).is_ok_and(|p| &p == path) && path.is_file()),
        })
        .collect()
}

/// Preview the mapped range in an open buffer when available, otherwise the
/// PR-head line numbers in the file on disk.
fn list_preview<'a>(editor: &'a Editor, entry: &'a ListEntry) -> Option<FileLocation<'a>> {
    for doc in editor.documents() {
        if let Some(block) = doc
            .review
            .blocks
            .iter()
            .find(|b| b.thread.id == entry.thread.id)
        {
            let text = doc.text();
            let start = text.char_to_line(block.range.start);
            let end = text.char_to_line(block.range.end.saturating_sub(1).max(block.range.start));
            return Some((PathOrId::Id(doc.id()), Some((start, end))));
        }
    }
    let lines = entry.thread.lines.as_ref()?;
    Some((
        entry.path.as_deref()?.into(),
        Some((lines.start - 1, lines.end - 2)),
    ))
}

/// First non-empty line of the opening comment, bounded so the location column
/// (whose end, `file:line`, matters most) keeps its room in the picker.
fn first_line(thread: &review::Thread) -> String {
    const WIDTH: usize = 40;
    let line = thread
        .comments
        .first()
        .and_then(|c| c.body.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim();
    if line.chars().count() <= WIDTH {
        return line.to_owned();
    }
    let mut short: String = line.chars().take(WIDTH - 1).collect();
    short.push('…');
    short
}

fn list_location(thread: &review::Thread) -> String {
    match &thread.lines {
        Some(lines) if lines.end - lines.start > 1 => {
            format!("{}:{}-{}", thread.path, lines.start, lines.end - 1)
        }
        Some(lines) => format!("{}:{}", thread.path, lines.start),
        // parse_thread records why a discussion is not placed on current code.
        None if thread.location.contains("(outdated") => format!("{} (outdated)", thread.path),
        None => format!("{} (no line)", thread.path),
    }
}

pub(crate) fn list(cx: &mut compositor::Context) {
    synchronize(cx.editor);
    let Some(review) = cx.editor.review.review.clone() else {
        cx.editor.set_status(cx.editor.review.status.clone());
        return;
    };
    if review.threads.is_empty() {
        cx.editor.set_status(review.summary());
        return;
    }
    let generation = cx.editor.review.generation;
    let context = cx.editor.review.context.clone();
    let entries = list_entries(&review, context.as_ref().map(|c| c.root.as_path()));
    let selected = cx.editor.review.selected.unwrap_or(0);
    cx.jobs.callback(async move {
        Ok(job::Callback::EditorCompositor(Box::new(
            move |_, compositor| {
                let picker = Picker::new(
                    [
                        PickerColumn::new("comment", |entry: &ListEntry, _| {
                            first_line(&entry.thread).into()
                        }),
                        PickerColumn::new("location", |entry: &ListEntry, _| {
                            list_location(&entry.thread).into()
                        }),
                        PickerColumn::new("discussion", |entry: &ListEntry, _| {
                            entry.thread.title().into()
                        }),
                    ],
                    1,
                    entries,
                    (),
                    move |cx, entry, _| {
                        if context
                            .as_ref()
                            .is_some_and(|c| cx.editor.review.accepts(generation, c))
                            && current_context(cx.editor) == context
                        {
                            select(cx.editor, entry.index);
                        } else {
                            cx.editor
                                .set_error("Review context changed; reopen :review-list");
                        }
                    },
                )
                .with_initial_cursor(selected as u32)
                .with_preview(list_preview);
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tracks_worktree_branch_oid_and_remote_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        let git = dir.path().join("git");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(root.join(".git"), format!("gitdir: {}", git.display())).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(git.join("refs/heads/topic"), "aaa\n").unwrap();
        let first = context_at(&root).unwrap();
        fs::write(git.join("refs/heads/topic"), "bbb\n").unwrap();
        assert_ne!(Some(first), context_at(&root));
        let second = context_at(&root);
        fs::write(git.join("config"), "remote changed").unwrap();
        assert_ne!(second, context_at(&root));
        fs::write(git.join("HEAD"), "detached\n").unwrap();
        assert!(context_at(&root).is_none());
    }

    #[test]
    fn list_rows_and_previews_stay_inside_the_repository() {
        use helix_view::review::{Comment, Review, Thread};
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        fs::write(root.join("a.rs"), "x\n").unwrap();
        let thread = |path: &str, lines| {
            Arc::new(Thread {
                id: path.into(),
                path: path.into(),
                lines,
                location: String::new(),
                resolved: false,
                comments: vec![Comment {
                    author: "a".into(),
                    body: "\n  first line\nsecond".into(),
                }],
                diff: String::new(),
                url: String::new(),
                ..Default::default()
            })
        };
        let review = Review {
            label: "o/r#1 @ abc".into(),
            threads: vec![
                thread("a.rs", Some(1..2)),
                thread("../a.rs", Some(1..2)),
                thread("missing.rs", Some(2..5)),
                thread("a.rs", None),
            ],
            sources: Default::default(),
            ..Default::default()
        };
        let entries = list_entries(&review, Some(&root));
        assert_eq!(entries[0].path, Some(root.join("a.rs")));
        assert!(entries[1].path.is_none());
        assert!(entries[2].path.is_none());
        assert_eq!(list_location(&review.threads[0]), "a.rs:1");
        assert_eq!(list_location(&review.threads[2]), "missing.rs:2-4");
        assert_eq!(list_location(&review.threads[3]), "a.rs (no line)");
        let outdated = Thread {
            location: "a.rs:? (outdated, RIGHT, original 3, commit abc)".into(),
            ..(*review.threads[3]).clone()
        };
        assert_eq!(list_location(&outdated), "a.rs (outdated)");
        assert_eq!(first_line(&review.threads[0]), "first line");
        let long = Thread {
            comments: vec![Comment {
                author: "a".into(),
                body: "x".repeat(100),
            }],
            ..(*review.threads[0]).clone()
        };
        assert_eq!(first_line(&long), format!("{}…", "x".repeat(39)));
    }

    #[test]
    fn missing_context_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        // Stray ancestor .git directories (e.g. /tmp/.git) change the answer.
        if !dir.path().ancestors().any(|d| d.join(".git").exists()) {
            assert!(missing_context(dir.path()).contains("not inside a Git repository"));
        }
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/unborn\n").unwrap();
        assert!(context_at(&root).is_none());
        assert!(missing_context(&root).contains("cannot read branch unborn"));
        fs::write(root.join(".git/HEAD"), "0123abcd\n").unwrap();
        assert!(missing_context(&root.join("sub")).contains("detached HEAD"));
    }
}

#[cfg(all(test, feature = "integration"))]
mod editor_tests {
    use super::*;
    use crate::config::Config;
    use helix_core::syntax;
    use helix_view::review::{Comment, Review, Thread};
    use std::collections::HashMap;

    async fn settle(editor: &mut Editor, jobs: &mut job::Jobs) {
        let mut compositor =
            crate::compositor::Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        while editor.documents.values().any(|d| d.review.task.is_some()) {
            let callback =
                tokio::time::timeout(std::time::Duration::from_secs(5), jobs.callbacks.recv())
                    .await
                    .unwrap()
                    .unwrap();
            jobs.handle_callback(editor, &mut compositor, Ok(Some(callback)));
        }
    }

    pub(super) fn fixture_editor() -> Editor {
        let config = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));
        let handlers = crate::handlers::setup(config.clone());
        Editor::new(
            helix_view::graphics::Rect::new(0, 0, 120, 40),
            Arc::new(helix_view::theme::Loader::new(&[])),
            Arc::new(arc_swap::ArcSwap::from_pointee(syntax::Loader::default())),
            Arc::new(arc_swap::access::Map::new(config, |c: &Config| &c.editor)),
            handlers,
        )
    }

    /// A checkout reached through a symlinked directory must still show its
    /// discussions, and navigation must reuse that buffer instead of opening a
    /// duplicate under the resolved path.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_opened_through_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap().join("repo");
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(root.join(".git/refs/heads/topic"), "aaa\n").unwrap();
        let source = "one\ntwo\nthree\nfour\nfive\n";
        fs::write(root.join("a.txt"), source).unwrap();
        let link = canonical(dir.path()).unwrap().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let mut editor_value = fixture_editor();
        let mut jobs = job::Jobs::new();
        let editor = &mut editor_value;
        editor
            .open(&link.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        assert_eq!(doc!(editor).path(), Some(&link.join("a.txt")));
        editor.review.enabled = true;
        editor.review.context = current_context(editor);
        assert_eq!(editor.review.context.as_ref().unwrap().root, root);
        editor.review.status = "fixture".into();
        editor.review.review = Some(Arc::new(Review {
            label: "fixture#1".into(),
            threads: vec![Arc::new(Thread {
                id: "t".into(),
                path: "a.txt".into(),
                lines: Some(2..3),
                location: "a.txt:2".into(),
                resolved: false,
                comments: vec![Comment {
                    author: "reviewer".into(),
                    body: "body".into(),
                }],
                diff: String::new(),
                url: String::new(),
                ..Default::default()
            })],
            sources: HashMap::from([("a.txt".into(), source.into())]),
            ..Default::default()
        }));
        attach_documents(editor);
        settle(editor, &mut jobs).await;
        assert_eq!(doc!(editor).review.blocks.len(), 1);
        let linked = doc!(editor).id();
        let documents = editor.documents().count();
        select(editor, 0);
        settle(editor, &mut jobs).await;
        assert_eq!(doc!(editor).id(), linked);
        assert_eq!(editor.documents().count(), documents);
        assert_eq!(under_cursor(editor), Some(0));
        // :review-list previews the mapped range of the open buffer.
        let review = editor.review.review.clone().unwrap();
        let entries = list_entries(&review, Some(&root));
        assert_eq!(
            entries[0].path.as_deref(),
            Some(root.join("a.txt").as_path())
        );
        match list_preview(editor, &entries[0]) {
            Some((PathOrId::Id(id), Some(lines))) => {
                assert_eq!((id, lines), (linked, (1, 1)));
            }
            _ => panic!("expected a document preview"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixture_navigation_edits_context_switch_and_symlink_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(root.join(".git/refs/heads/topic"), "aaa\n").unwrap();
        let source = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        fs::write(root.join("a.txt"), source).unwrap();
        let config = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));
        let handlers = crate::handlers::setup(config.clone());
        let mut editor_value = Editor::new(
            helix_view::graphics::Rect::new(0, 0, 120, 40),
            Arc::new(helix_view::theme::Loader::new(&[])),
            Arc::new(arc_swap::ArcSwap::from_pointee(syntax::Loader::default())),
            Arc::new(arc_swap::access::Map::new(config, |c: &Config| &c.editor)),
            handlers,
        );
        let mut jobs = job::Jobs::new();
        editor_value
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        let editor = &mut editor_value;
        let threads = (0..3)
            .map(|i| {
                Arc::new(Thread {
                    id: i.to_string(),
                    path: "a.txt".into(),
                    lines: if i == 2 { None } else { Some(4..5) },
                    location: "a.txt:4 (fixture)".into(),
                    resolved: false,
                    comments: vec![Comment {
                        author: format!("reviewer{i}"),
                        body: format!("body {i}"),
                    }],
                    diff: "@@ original".into(),
                    url: String::new(),
                    ..Default::default()
                })
            })
            .collect();
        editor.review.enabled = true;
        editor.review.context = context_at(&root);
        editor.review.status = "fixture".into();
        editor.review.review = Some(Arc::new(Review {
            label: "fixture#1".into(),
            threads,
            sources: HashMap::from([("a.txt".into(), source.into())]),
            ..Default::default()
        }));
        attach_documents(editor);
        // A mapping result already queued before an edit must not paint onto
        // the new document version, even though the repository epoch is equal.
        let stale = tokio::time::timeout(std::time::Duration::from_secs(5), jobs.callbacks.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let (view, doc) = current!(editor);
            let change =
                Transaction::change(doc.text(), [(0, 0, Some("temporary\n".into()))].into_iter());
            doc.apply(&change, view.id);
            let undo = Transaction::change(doc.text(), [(0, 10, None)].into_iter());
            doc.apply(&undo, view.id);
        }
        let mut compositor =
            crate::compositor::Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        jobs.handle_callback(editor, &mut compositor, Ok(Some(stale)));
        assert!(doc!(editor).review.blocks.is_empty());
        attach_documents(editor);
        select(editor, 1); // navigation waits for the current mapping result
        assert!(editor.review.pending_selection.is_some());
        settle(editor, &mut jobs).await;
        assert_eq!(under_cursor(editor), Some(1));
        assert_eq!(doc!(editor).review.blocks.len(), 2);
        select(editor, 0);
        assert_eq!(under_cursor(editor), Some(0));
        select(editor, 1);
        assert_eq!(under_cursor(editor), Some(1));
        let original_id = doc!(editor).id();
        // A delayed selection belongs to the requesting split, even if the
        // user focuses another split with the same document and cursor.
        let requesting_view = view!(editor).id;
        doc_mut!(editor).review.stamp = None;
        select(editor, 2);
        assert!(editor.review.pending_selection.is_some());
        editor.switch(original_id, Action::VerticalSplit);
        let other_view = view!(editor).id;
        assert_ne!(other_view, requesting_view);
        let requesting_selection = doc!(editor).selection(requesting_view).clone();
        doc_mut!(editor).set_selection(other_view, requesting_selection);
        let cursor = doc!(editor).selection(other_view).clone();
        settle(editor, &mut jobs).await;
        assert_eq!(view!(editor).id, other_view);
        assert_eq!(doc!(editor).id(), original_id);
        assert_eq!(doc!(editor).selection(other_view), &cursor);
        assert!(editor.review.pending_selection.is_none());
        editor.focus(requesting_view);
        select(editor, 1);
        {
            let (view, doc) = current!(editor);
            let change =
                Transaction::change(doc.text(), [(0, 0, Some("unsaved\n".into()))].into_iter());
            doc.apply(&change, view.id);
            assert!(doc.review.blocks.is_empty());
        }
        attach_documents(editor);
        settle(editor, &mut jobs).await;
        assert_eq!(doc!(editor).review.blocks[0].range, 22..27);
        {
            let (view, doc) = current!(editor);
            let change =
                Transaction::change(doc.text(), [(22, 26, Some("changed".into()))].into_iter());
            doc.apply(&change, view.id);
        }
        attach_documents(editor);
        settle(editor, &mut jobs).await;
        assert!(doc!(editor).review.blocks.is_empty());
        select(editor, 2);
        assert!(doc!(editor).readonly);
        assert!(doc!(editor)
            .text()
            .to_string()
            .contains("Location unavailable"));
        assert!(doc!(editor).text().to_string().contains("body 2"));
        let discussion_id = doc!(editor).id();
        open_thread(editor, 1);
        assert_eq!(doc!(editor).id(), discussion_id);
        // Never overwrite a reader that the user has edited or saved as a file.
        {
            let (view, doc) = current!(editor);
            let change = Transaction::change(
                doc.text(),
                [(0, 0, Some("user notes\n".into()))].into_iter(),
            );
            doc.apply(&change, view.id);
        }
        open_thread(editor, 0);
        assert_ne!(doc!(editor).id(), discussion_id);
        assert!(editor
            .document(discussion_id)
            .unwrap()
            .text()
            .to_string()
            .starts_with("user notes"));
        let saved_reader = doc!(editor).id();
        doc_mut!(editor).set_path(Some(&root.join("notes.txt")));
        open_thread(editor, 0);
        assert_ne!(doc!(editor).id(), saved_reader);
        assert_eq!(
            editor.document(saved_reader).unwrap().path(),
            Some(&root.join("notes.txt"))
        );
        editor.switch(original_id, Action::Replace);
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("a.txt"), source).unwrap();
            std::os::unix::fs::symlink(outside.path().join("a.txt"), root.join("link.txt"))
                .unwrap();
            let mut review = (*editor.review.review.clone().unwrap()).clone();
            let mut thread = (*review.threads[0]).clone();
            thread.path = "link.txt".into();
            review.threads = vec![Arc::new(thread)];
            review.sources.insert("link.txt".into(), source.into());
            editor.review.review = Some(Arc::new(review));
            editor
                .open(&root.join("link.txt"), Action::Replace)
                .unwrap();
            attach_documents(editor);
            settle(editor, &mut jobs).await;
            assert!(doc!(editor).review.blocks.is_empty());
        }
        let old_context = editor.review.context.clone().unwrap();
        let old_epoch = editor.review.generation;
        // Detached HEAD clears comments immediately and does not launch a fetch.
        fs::write(root.join(".git/HEAD"), "bbb\n").unwrap();
        synchronize(editor);
        assert!(editor.review.review.is_none());
        assert!(!editor.review.accepts(old_epoch, &old_context));
        assert!(editor
            .documents
            .values()
            .all(|d| d.review.blocks.is_empty()));
        assert!(editor.review.task.is_none());
    }
}
