//! Project notes: capture a task or note from the code under the cursor into
//! the project's Markdown notes file, list the tasks of one or all projects,
//! and go from an entry back to its code.

pub(crate) mod format;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use helix_core::{syntax::QueryIterEvent, tree_sitter::Node, Selection, Transaction};
use helix_view::{editor::Action, notes::Capture, Align, DocumentId, Editor};

use crate::{
    alt,
    commands::Context,
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

use format::{Link, Status, Task};

fn notes_directory(editor: &Editor) -> PathBuf {
    helix_stdx::path::expand_tilde(editor.config().notes.directory.clone()).into_owned()
}

/// The workspace root of the current buffer, else of the working directory.
fn current_project(editor: &Editor) -> PathBuf {
    let doc = doc!(editor);
    // A notes buffer belongs to the project its front matter names.
    let directory = helix_stdx::path::canonicalize(notes_directory(editor));
    if doc.path().is_some_and(|path| path.starts_with(&directory)) {
        if let Some(root) = format::project_root(&doc.text().to_string()) {
            return root;
        }
    }
    match doc.path().and_then(|path| path.parent()) {
        Some(dir) => helix_loader::find_workspace_in(dir).0,
        None => helix_loader::find_workspace().0,
    }
}

/// The text of a file, from its buffer when it is open.
fn read(editor: &Editor, path: &Path) -> Option<String> {
    match editor.document_by_path(path) {
        Some(doc) => Some(doc.text().to_string()),
        None => fs::read_to_string(path).ok(),
    }
}

/// The notes file of `project`: `<name>.md` in the notes directory, or
/// `<parent>-<name>.md` when another project with the same name has it.
fn notes_file(editor: &Editor, project: &Path) -> anyhow::Result<PathBuf> {
    let directory = notes_directory(editor);
    let name = project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    let mut candidates = vec![format!("{name}.md")];
    if let Some(parent) = project.parent().and_then(Path::file_name) {
        candidates.push(format!("{}-{name}.md", parent.to_string_lossy()));
    }
    for candidate in candidates {
        let path = helix_stdx::path::canonicalize(directory.join(candidate));
        match read(editor, &path).map(|text| format::project_root(&text)) {
            None => return Ok(path),
            Some(Some(root)) if root == project => return Ok(path),
            Some(None) => return Ok(path),
            Some(Some(_)) => {}
        }
    }
    anyhow::bail!(
        "Notes files for another project named {name} exist in {}; rename one",
        directory.display()
    )
}

fn display(path: &Path) -> String {
    helix_stdx::path::fold_home_dir(path)
        .to_string_lossy()
        .into_owned()
}

/// The short commit that `HEAD` names in `root`, if it is a Git repository.
fn head_commit(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !commit.is_empty()).then_some(commit)
}

fn field<'a>(node: &Node<'a>, name: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if cursor.field_name() == Some(name) {
            return Some(cursor.node());
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
}

/// A definition in a buffer: its name and the line range of its whole node.
struct Symbol {
    name: String,
    start_line: usize,
    end_line: usize,
}

/// The definitions that the language's `tags.scm` finds. Queries capture
/// either the whole definition, which has a `name` field, or only the name,
/// whose parent is the definition.
fn symbols(doc: &helix_view::Document, loader: &helix_core::syntax::Loader) -> Vec<Symbol> {
    let Some(syntax) = doc.syntax() else {
        return Vec::new();
    };
    let text = doc.text().slice(..);
    let mut iter = syntax.tags(text, loader, ..);
    let mut symbols = Vec::new();
    while let Some(event) = iter.next() {
        let QueryIterEvent::Match(mat) = event else {
            continue;
        };
        let Some(query) = loader.tag_query(iter.current_language()) else {
            continue;
        };
        if !query
            .query
            .capture_name(mat.capture)
            .starts_with("definition.")
        {
            continue;
        }
        let (definition, name) = match field(&mat.node, "name") {
            Some(name) => (mat.node.clone(), name),
            None => match mat.node.parent() {
                Some(parent) => (parent, mat.node.clone()),
                None => continue,
            },
        };
        let name_range = name.byte_range();
        let name = text
            .byte_slice(name_range.start as usize..name_range.end as usize)
            .to_string();
        if name.is_empty() || name.contains('\n') || name.chars().count() > 100 {
            continue;
        }
        let range = definition.byte_range();
        symbols.push(Symbol {
            name,
            start_line: text.byte_to_line(range.start as usize),
            end_line: text.byte_to_line(range.end as usize),
        });
    }
    symbols
}

/// The innermost definition containing `line`.
fn enclosing_symbol(symbols: &[Symbol], line: usize) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
}

/// `note_capture`: a draft below the current split that becomes an entry
/// under the configured heading of the project's notes file on `:w`.
pub fn note_capture(cx: &mut Context) {
    let editor = &mut *cx.editor;
    prune(editor);
    let (view, doc) = current_ref!(editor);
    let Some(path) = doc.path().cloned() else {
        return editor.set_error("Save the buffer first: a capture links to a file");
    };
    let project = current_project(editor);
    let text = doc.text().slice(..);
    let (start, end) = doc.selection(view.id).primary().line_range(text);
    let anchored: Vec<String> = (start..=end).map(|l| text.line(l).to_string()).collect();
    let snippet = format::snippet(anchored.iter().map(String::as_str));
    let loader = editor.syn_loader.load();
    let symbols = symbols(doc, &loader);
    let link = Link {
        path: path
            .strip_prefix(&project)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned(),
        start,
        end,
        symbol: enclosing_symbol(&symbols, start).map(|s| s.name.clone()),
        commit: head_commit(&project),
        snippet,
    };
    let notes_file = match notes_file(editor, &project) {
        Ok(file) => file,
        Err(err) => return editor.set_error(err.to_string()),
    };
    let heading = editor.config().notes.heading.clone();
    let anchor = format::render_anchor(&link);
    let draft = format::capture_draft(&anchor, &heading, &display(&notes_file));

    let id = editor.new_file(Action::HorizontalSplit);
    let (view, doc) = current!(editor);
    doc.set_virtual_name(Some(format!("[note] {}", link.describe())));
    doc.set_soft_wrap_override(Some(true));
    let _ = doc.set_language_by_language_id("markdown", &loader);
    let transaction = Transaction::change(doc.text(), [(0, 0, Some(draft.into()))].into_iter())
        .with_selection(Selection::point(format::CAPTURE_CURSOR));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    editor.notes.captures.insert(
        id,
        Capture {
            project,
            notes_file,
            anchor,
        },
    );
    editor.mode = helix_view::document::Mode::Insert;
    editor.set_status(format!(
        "Capturing {} · :w saves under \"{heading}\", :bc! discards",
        link.describe()
    ));
}

pub(crate) fn is_capture(editor: &Editor, doc: DocumentId) -> bool {
    editor.notes.captures.contains_key(&doc)
}

/// Reloads the buffer of `doc` from disk, keeping its views' cursors.
fn reload(editor: &mut Editor, doc: DocumentId) {
    let focused = view!(editor).id;
    let doc = doc_mut!(editor, &doc);
    let view_id = doc.selections().keys().next().copied().unwrap_or_else(|| {
        doc.ensure_view_init(focused);
        focused
    });
    let view = view_mut!(editor, view_id);
    view.sync_changes(doc);
    if let Err(err) = doc.reload(view, &editor.diff_providers) {
        editor.set_error(format!("Cannot reload the notes buffer: {err}"));
    }
}

/// Replaces the text of `path`, on disk and in its buffer when it is open.
/// Refuses when the buffer has unsaved changes.
fn write_notes(editor: &mut Editor, path: &Path, text: &str) -> anyhow::Result<()> {
    let open = editor
        .document_by_path(path)
        .map(|doc| (doc.id(), doc.is_modified()));
    if let Some((_, true)) = open {
        anyhow::bail!(
            "{} has unsaved changes; save or discard them first",
            display(path)
        );
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text)?;
    if let Some((doc, _)) = open {
        reload(editor, doc);
    }
    Ok(())
}

/// `:write` in a capture draft: adds the entry and closes the draft.
pub(crate) fn save_capture(editor: &mut Editor) {
    let doc = doc!(editor);
    let id = doc.id();
    let draft = doc.text().to_string();
    let Some(capture) = editor.notes.captures.get(&id).cloned() else {
        return editor.set_error("Not a capture draft; start one with note_capture");
    };
    let Some(entry) = format::entry(format::capture_body(&draft), &capture.anchor) else {
        return editor.set_error("Write the task or note above the capture line first");
    };
    let heading = editor.config().notes.heading.clone();
    let text = read(editor, &capture.notes_file).unwrap_or_else(|| {
        let name = capture
            .notes_file
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        format::new_file(&capture.project, &name, &heading)
    });
    let (at, insertion) = format::insert_under_heading(&text, &heading, &entry);
    let mut updated: String = text.chars().take(at).collect();
    updated.push_str(&insertion);
    updated.extend(text.chars().skip(at));
    if let Err(err) = write_notes(editor, &capture.notes_file, &updated) {
        return editor.set_error(format!("Note not saved: {err}"));
    }
    editor.notes.captures.remove(&id);
    let _ = editor.close_document(id, true);
    editor.enter_normal_mode();
    editor.set_status(format!(
        "Saved under \"{heading}\" in {}",
        display(&capture.notes_file)
    ));
}

/// Forgets drafts whose buffers are closed.
fn prune(editor: &mut Editor) {
    let documents = &editor.documents;
    editor
        .notes
        .captures
        .retain(|id, _| documents.contains_key(id));
}

fn open_at(editor: &mut Editor, path: &Path, line: usize, action: Action) -> Option<DocumentId> {
    let (view, doc) = current!(editor);
    doc.append_changes_to_history(view);
    view.jumps.push((doc.id(), doc.selection(view.id).clone()));
    let id = match editor.open(path, action) {
        Ok(id) => id,
        Err(err) => {
            editor.set_error(format!("Cannot open {}: {err}", display(path)));
            return None;
        }
    };
    let view = view_mut!(editor);
    let doc = doc_mut!(editor, &id);
    let line = line.min(doc.text().len_lines().saturating_sub(1));
    let pos = doc.text().line_to_char(line);
    doc.set_selection(view.id, Selection::point(pos));
    if action.align_view(view, id) {
        helix_view::align_view(doc, view, Align::Center);
    }
    Some(id)
}

/// `note_open`: the current project's notes file.
pub fn note_open(cx: &mut Context) {
    let project = current_project(cx.editor);
    let path = match notes_file(cx.editor, &project) {
        Ok(path) => path,
        Err(err) => return cx.editor.set_error(err.to_string()),
    };
    if !path.exists() {
        let heading = cx.editor.config().notes.heading.clone();
        let name = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(err) = write_notes(
            cx.editor,
            &path,
            &format::new_file(&project, &name, &heading),
        ) {
            return cx
                .editor
                .set_error(format!("Cannot create {}: {err}", display(&path)));
        }
    }
    if let Err(err) = cx.editor.open(&path, Action::Replace) {
        cx.editor
            .set_error(format!("Cannot open {}: {err}", display(&path)));
    }
}

/// Opens the code that `link` points to. Follows the quoted line when lines
/// moved, else the symbol, else keeps the recorded line and says so.
fn goto_code(editor: &mut Editor, project: &Path, link: &Link, action: Action) {
    let path = project.join(&link.path);
    if !path.is_file() {
        return editor.set_error(format!(
            "{} no longer exists (moved or renamed?)",
            link.path
        ));
    }
    let Some(id) = open_at(editor, &path, link.start, action) else {
        return;
    };
    let doc = doc!(editor, &id);
    let text = doc.text();
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    let still =
        |line: usize, snippet: &str| lines.get(line).is_some_and(|text| text.trim() == snippet);
    let (line, status) = match &link.snippet {
        None => (link.start, None),
        Some(snippet) if still(link.start, snippet) => (link.start, None),
        Some(snippet) => {
            match format::relocate(lines.iter().map(String::as_str), link.start, snippet) {
                Some(line) => (
                    line,
                    Some(format!(
                        "The code moved: line {} is now {}",
                        link.start + 1,
                        line + 1
                    )),
                ),
                None => {
                    let loader = editor.syn_loader.load();
                    let symbol = link.symbol.as_ref().and_then(|name| {
                        symbols(doc, &loader)
                            .into_iter()
                            .filter(|s| &s.name == name)
                            .min_by_key(|s| s.start_line.abs_diff(link.start))
                    });
                    match symbol {
                        Some(symbol) => (
                            symbol.start_line,
                            Some(format!(
                                "The code changed since the capture; went to `{}`",
                                symbol.name
                            )),
                        ),
                        None => (
                            link.start,
                            Some(format!(
                                "The code changed since the capture; line {} may be stale",
                                link.start + 1
                            )),
                        ),
                    }
                }
            }
        }
    };
    if line != link.start {
        let view = view_mut!(editor);
        let doc = doc_mut!(editor, &id);
        let line = line.min(doc.text().len_lines().saturating_sub(1));
        doc.set_selection(view.id, Selection::point(doc.text().line_to_char(line)));
        helix_view::align_view(doc, view, Align::Center);
    }
    if let Some(status) = status {
        editor.set_status(status);
    }
}

/// `note_goto_code`: from an entry in a notes buffer to its code.
pub fn note_goto_code(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().to_string();
    let Some(project) = format::project_root(&text) else {
        return cx
            .editor
            .set_error("Not a notes file: its front matter names no project");
    };
    let line = doc.text().char_to_line(
        doc.selection(view.id)
            .primary()
            .cursor(doc.text().slice(..)),
    );
    match format::link_at(&text, line) {
        Some(link) => goto_code(cx.editor, &project, &link, Action::Replace),
        None => cx
            .editor
            .set_error("No code: link in the entry at the cursor"),
    }
}

/// Toggles the task on `line` of the notes file at `path`.
fn toggle_task(editor: &mut Editor, path: &Path, line: usize) -> anyhow::Result<Status> {
    let text =
        read(editor, path).ok_or_else(|| anyhow::anyhow!("cannot read {}", display(path)))?;
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(current) = lines.get(line).copied() else {
        anyhow::bail!("the task is no longer on line {}", line + 1);
    };
    let ending = &current[current.trim_end_matches(['\n', '\r']).len()..];
    let toggled = format::toggle(current.trim_end_matches(['\n', '\r']))
        .ok_or_else(|| anyhow::anyhow!("line {} is not a task", line + 1))?;
    let toggled = format!("{toggled}{ending}");
    lines[line] = &toggled;
    let updated = lines.concat();
    write_notes(editor, path, &updated)?;
    Ok(format::tasks(&toggled)
        .first()
        .map_or(Status::Open, |task| task.status))
}

/// `note_toggle_task`: toggles the task at the cursor between open and done.
/// Edits the buffer, so the change saves with it.
pub fn note_toggle_task(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().to_string();
    let cursor_line = doc.text().char_to_line(
        doc.selection(view.id)
            .primary()
            .cursor(doc.text().slice(..)),
    );
    let Some(line) = format::task_at(&text, cursor_line) else {
        return cx.editor.set_error("No task at the cursor");
    };
    let rope = doc.text();
    let start = rope.line_to_char(line);
    let content = rope.line(line).to_string();
    let content = content.trim_end_matches(['\n', '\r']);
    let Some(toggled) = format::toggle(content) else {
        return;
    };
    let end = start + content.chars().count();
    let transaction = Transaction::change(rope, [(start, end, Some(toggled.into()))].into_iter());
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
}

#[derive(Debug, Clone)]
struct TaskItem {
    file: PathBuf,
    project: Option<PathBuf>,
    /// The linked source file, for the preview.
    code: Option<PathBuf>,
    name: String,
    task: Task,
}

fn notes_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![directory.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn load_tasks(editor: &Editor, all: bool) -> anyhow::Result<Vec<TaskItem>> {
    let files = if all {
        notes_files(&notes_directory(editor))
    } else {
        vec![notes_file(editor, &current_project(editor))?]
    };
    let mut items = Vec::new();
    for file in files {
        let file = helix_stdx::path::canonicalize(file);
        let Some(text) = read(editor, &file) else {
            continue;
        };
        let project = format::project_root(&text);
        let name = file
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        items.extend(
            format::tasks(&text)
                .into_iter()
                .filter(|task| task.status == Status::Open)
                .map(|task| TaskItem {
                    file: file.clone(),
                    code: project
                        .as_ref()
                        .zip(task.link.as_ref())
                        .map(|(project, link)| project.join(&link.path)),
                    project: project.clone(),
                    name: name.clone(),
                    task,
                }),
        );
    }
    Ok(items)
}

fn open_item(editor: &mut Editor, item: &TaskItem, action: Action) {
    match (&item.project, &item.task.link) {
        (Some(project), Some(link)) => goto_code(editor, project, link, action),
        _ => {
            open_at(editor, &item.file, item.task.line, action);
        }
    }
}

fn show_tasks(cx: &mut Context, all: bool, cursor: u32) {
    let items = match load_tasks(cx.editor, all) {
        Ok(items) => items,
        Err(err) => return cx.editor.set_error(err.to_string()),
    };
    if items.is_empty() {
        let scope = if all { "any project" } else { "this project" };
        return cx.editor.set_status(format!("No open tasks in {scope}"));
    }
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_, compositor| push_picker(compositor, items, all, cursor),
        )))
    });
}

fn push_picker(
    compositor: &mut compositor::Compositor,
    items: Vec<TaskItem>,
    all: bool,
    cursor: u32,
) {
    let mut columns = vec![PickerColumn::new("task", |item: &TaskItem, _| {
        item.task.text.as_str().into()
    })];
    if all {
        columns.push(PickerColumn::new("project", |item: &TaskItem, _| {
            item.name.as_str().into()
        }));
    }
    columns.push(PickerColumn::new("code", |item: &TaskItem, _| {
        item.task
            .link
            .as_ref()
            .map(Link::describe)
            .unwrap_or_default()
            .into()
    }));
    let picker = Picker::new(columns, 0, items, (), |cx, item: &TaskItem, action| {
        open_item(cx.editor, item, action)
    })
    .with_initial_cursor(cursor)
    .with_preview(|_, item: &TaskItem| match (&item.code, &item.task.link) {
        (Some(code), Some(link)) => Some((code.as_path().into(), Some((link.start, link.end)))),
        _ => Some((
            item.file.as_path().into(),
            Some((item.task.line, item.task.line)),
        )),
    })
    .with_key_action(alt!('n'), |cx, item: &TaskItem| {
        open_at(cx.editor, &item.file, item.task.line, Action::Replace);
        true
    })
    .with_key_action(alt!('x'), move |cx, item: &TaskItem| {
        match toggle_task(cx.editor, &item.file, item.task.line) {
            Ok(_) => cx.editor.set_status(format!("Done: {}", item.task.text)),
            Err(err) => {
                cx.editor
                    .set_error(format!("Cannot toggle the task: {err}"));
                return false;
            }
        }
        let items = match load_tasks(cx.editor, all) {
            Ok(items) if !items.is_empty() => items,
            _ => return true,
        };
        let cursor = items
            .iter()
            .position(|other| other.file == item.file && other.task.line >= item.task.line)
            .unwrap_or(items.len() - 1) as u32;
        cx.jobs.callback(async move {
            Ok(Callback::EditorCompositor(Box::new(
                move |_, compositor| push_picker(compositor, items, all, cursor),
            )))
        });
        true
    });
    compositor.push(Box::new(overlaid(picker)));
}

/// `note_tasks`: the open tasks of the current project.
pub fn note_tasks(cx: &mut Context) {
    show_tasks(cx, false, 0);
}

/// `note_tasks_all`: the open tasks of every project in the notes directory.
pub fn note_tasks_all(cx: &mut Context) {
    show_tasks(cx, true, 0);
}
