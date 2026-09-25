//! Git status, staging and commits for the focused buffer's repository.
//!
//! Git runs in the background with fixed argument lists and literal pathspecs.
//! Nothing is staged or committed except by an explicit command or picker
//! action; opening views, navigating and saving files never do. A commit is
//! created only while the branch, HEAD and the index still match the draft
//! that shows them.
mod push;
pub(crate) mod status;

pub(crate) use push::push;

use crate::{
    alt, compositor, job, process,
    review::{canonical, current_path, missing_context, read_small, relative_key, repository_at},
    ui::{overlay::overlaid, picker::PathOrId, Picker, PickerColumn},
};
use anyhow::anyhow;
use helix_core::{Selection, Transaction};
use helix_view::{
    editor::Action,
    git::{commit_message, Draft, Running, Snapshot, COMMIT_SEPARATOR},
    review::safe_text,
    theme::Style,
    DocumentId, Editor,
};
use status::{Change, Part, Status};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tui::text::Span;

/// Status, diffs and staging.
const QUICK: Duration = Duration::from_secs(30);
/// Commits: hooks may run formatters and tests.
const HOOKS: Duration = Duration::from_secs(600);
/// Largest diff shown in a commit draft.
const DRAFT_DIFF: usize = 512 * 1024;

/// A repository found from a path without running Git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Repo {
    pub root: PathBuf,
    /// Checked-out branch; `None` for a detached HEAD.
    pub branch: Option<String>,
}

pub(crate) fn repo_at(path: &Path) -> Option<Repo> {
    let (root, git_dir) = repository_at(path)?;
    let head = String::from_utf8(read_small(&git_dir.join("HEAD"))?).ok()?;
    let branch = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned);
    Some(Repo { root, branch })
}

/// The repository of the focused buffer, of the focused commit draft, or of
/// the working directory.
pub(crate) fn current_repo(editor: &Editor) -> Result<Repo, String> {
    if let Some(draft) = editor.git.drafts.get(&doc!(editor).id()) {
        if let Some(repo) = repo_at(&draft.root) {
            return Ok(repo);
        }
    }
    let path = current_path(editor);
    repo_at(&path).ok_or_else(|| missing_context(&path).replacen("Reviews: ", "Git: ", 1))
}

/// Repository-relative key of `path` as Git names it: directories are
/// resolved, the file itself is not (it may be a tracked symlink, or not yet
/// on disk).
pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
    let real = canonical(path.parent()?).ok()?.join(path.file_name()?);
    relative_key(real.strip_prefix(root).ok()?)
}

/// Buffers of files in `root` with unsaved edits: Git only sees what is saved.
fn unsaved_buffers(editor: &Editor, root: &Path) -> Vec<(String, DocumentId)> {
    let mut unsaved: Vec<_> = editor
        .documents()
        .filter(|doc| doc.is_modified())
        .filter_map(|doc| Some((relative(root, doc.path()?)?, doc.id())))
        .collect();
    unsaved.sort();
    unsaved
}

pub(crate) async fn output(
    root: &Path,
    args: &[&str],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> anyhow::Result<process::Output> {
    let mut all = vec!["-c", "core.quotePath=false", "--literal-pathspecs"];
    all.extend_from_slice(args);
    process::output(root, "git", &all, input, timeout).await
}

/// First lines of a failure report, for the status line.
fn summary(text: &str, lines: usize) -> String {
    let lines: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(lines)
        .collect();
    lines.join(" · ")
}

/// Standard output of a successful Git command; failures carry Git's message.
pub(crate) async fn git(root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let out = output(root, args, None, QUICK).await?;
    anyhow::ensure!(
        out.success,
        "git {} failed: {}",
        args[0],
        summary(&out.message(), 3)
    );
    Ok(out.stdout)
}

pub(crate) async fn git_text(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    Ok(String::from_utf8(git(root, args).await?)?.trim().to_owned())
}

/// HEAD commit; `None` before the first commit.
async fn head(root: &Path) -> anyhow::Result<Option<String>> {
    let out = output(
        root,
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        None,
        QUICK,
    )
    .await?;
    Ok(out
        .success
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

async fn status(root: &Path) -> anyhow::Result<Status> {
    status::parse(
        &git(
            root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=normal",
            ],
        )
        .await?,
    )
}

const DIFF: &[&str] = &[
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

// ---------------------------------------------------------------------------
// Status view

enum Preview {
    File(PathBuf),
    Document(DocumentId),
}

struct Row {
    change: Change,
    preview: Option<Preview>,
    /// The buffer of an `Unsaved` row.
    document: Option<DocumentId>,
}

struct View {
    /// Diff previews live as long as the picker.
    _previews: tempfile::TempDir,
    styles: [Style; 5],
}

struct Loaded {
    status: Status,
    rows: Vec<Row>,
    previews: tempfile::TempDir,
}

async fn load(root: &Path, unsaved: Vec<(String, DocumentId)>) -> anyhow::Result<Loaded> {
    let status = status(root).await?;
    let diff = |cached: bool| async move {
        let mut args = DIFF.to_vec();
        if cached {
            args.push("--cached");
        }
        match output(root, &args, None, QUICK).await {
            Ok(out) if out.success => status::split_diff(&String::from_utf8_lossy(&out.stdout)),
            _ => Default::default(),
        }
    };
    let staged = diff(true).await;
    let unstaged = diff(false).await;
    let previews = tempfile::Builder::new().prefix("helix-git-").tempdir()?;
    let mut rows: Vec<_> = unsaved
        .into_iter()
        .map(|(path, document)| Row {
            change: Change {
                path,
                orig: None,
                part: Part::Unsaved,
                code: 'M',
                partial: false,
            },
            preview: Some(Preview::Document(document)),
            document: Some(document),
        })
        .collect();
    for (i, change) in status.changes.iter().enumerate() {
        let preview = match change.part {
            Part::Staged | Part::Unstaged => {
                let sections = if change.part == Part::Staged {
                    &staged
                } else {
                    &unstaged
                };
                let text: String = change
                    .orig
                    .iter()
                    .chain([&change.path])
                    .filter_map(|p| sections.get(p))
                    .map(String::as_str)
                    .collect();
                let file = previews.path().join(format!("{i}.diff"));
                (!text.is_empty() && std::fs::write(&file, text).is_ok())
                    .then_some(Preview::File(file))
            }
            Part::Untracked | Part::Conflict => Some(root.join(&change.path))
                .filter(|path| path.is_file())
                .map(Preview::File),
            Part::Unsaved => None,
        };
        rows.push(Row {
            change: change.clone(),
            preview,
            document: None,
        });
    }
    Ok(Loaded {
        status,
        rows,
        previews,
    })
}

fn branch_line(status: &Status) -> String {
    let branch = &status.branch;
    let mut line = branch
        .name
        .clone()
        .unwrap_or_else(|| "detached HEAD".into());
    if let Some(upstream) = &branch.upstream {
        line.push_str(&format!(" → {upstream}"));
        if branch.ahead > 0 {
            line.push_str(&format!(" ↑{}", branch.ahead));
        }
        if branch.behind > 0 {
            line.push_str(&format!(" ↓{}", branch.behind));
        }
    } else {
        line.push_str(" (no upstream)");
    }
    safe_text(&line)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IndexAction {
    Stage,
    StageWhole,
    Unstage,
}

/// `:git-status`: the changes picker.
pub(crate) fn status_view(cx: &mut compositor::Context) {
    let repo = match current_repo(cx.editor) {
        Ok(repo) => repo,
        Err(err) => return cx.editor.set_error(err),
    };
    let unsaved = unsaved_buffers(cx.editor, &repo.root);
    cx.jobs.callback(async move {
        let loaded = load(&repo.root, unsaved).await;
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| show(editor, compositor, repo.root, loaded, None),
        )))
    });
}

fn show(
    editor: &mut Editor,
    compositor: &mut compositor::Compositor,
    root: PathBuf,
    loaded: anyhow::Result<Loaded>,
    select: Option<(String, Part)>,
) {
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(err) => return editor.set_error(safe_text(&format!("Git status: {err:#}"))),
    };
    if current_repo(editor).map(|r| r.root).as_ref() != Ok(&root) {
        return editor.set_error("The focused repository changed; run :git-status again");
    }
    let count = |part| loaded.rows.iter().filter(|r| r.change.part == part).count();
    let counts = format!(
        "{} staged, {} unstaged, {} untracked{}{}",
        count(Part::Staged),
        count(Part::Unstaged),
        count(Part::Untracked),
        match count(Part::Conflict) {
            0 => String::new(),
            n => format!(", {n} conflicted"),
        },
        match count(Part::Unsaved) {
            0 => String::new(),
            n => format!(", {n} unsaved buffer(s)"),
        },
    );
    editor.set_status(format!(
        "{} · {counts} · Alt-s stage · Alt-a stage whole file · Alt-u unstage · Alt-c commit · Alt-p push",
        branch_line(&loaded.status)
    ));
    if loaded.rows.is_empty() {
        editor.set_status(format!(
            "{} · nothing to commit, working tree clean",
            branch_line(&loaded.status)
        ));
        return;
    }
    let cursor = select
        .and_then(|(path, part)| {
            let rows = &loaded.rows;
            rows.iter()
                .position(|r| r.change.path == path && r.change.part == part)
                .or_else(|| rows.iter().position(|r| r.change.path == path))
        })
        .unwrap_or(0);
    let theme = &editor.theme;
    let styles = [
        theme.get("warning"),
        theme.get("diff.delta.conflict"),
        theme.get("diff.plus"),
        theme.get("diff.delta"),
        theme.get("diff.minus"),
    ];
    let initial = loaded.status.branch.oid.is_none();
    let view = View {
        _previews: loaded.previews,
        styles,
    };
    let columns = [
        PickerColumn::new("state", |row: &Row, view: &View| {
            let style = view.styles[match row.change.part {
                Part::Unsaved => 0,
                Part::Conflict => 1,
                Part::Staged => 2,
                Part::Unstaged => 3,
                Part::Untracked => 4,
            }];
            Span::styled(row.change.describe(), style).into()
        }),
        PickerColumn::new("path", |row: &Row, _: &View| {
            safe_text(&row.change.display_path()).into()
        }),
    ];
    let open_root = root.clone();
    let action = |action: IndexAction| {
        let root = root.clone();
        move |cx: &mut compositor::Context, row: &Row| index_action(cx, &root, row, action, initial)
    };
    let picker = Picker::new(
        columns,
        1,
        loaded.rows,
        view,
        move |cx, row: &Row, action| {
            if let Some(document) = row.document {
                if cx.editor.documents.contains_key(&document) {
                    cx.editor.switch(document, action);
                }
                return;
            }
            let path = open_root.join(&row.change.path);
            if !path.is_file() {
                return cx.editor.set_error(format!(
                    "{} is not a file in the working tree",
                    row.change.path
                ));
            }
            if let Err(err) = cx.editor.open(&path, action) {
                cx.editor
                    .set_error(format!("Cannot open {}: {err}", row.change.path));
            }
        },
    )
    .with_initial_cursor(cursor as u32)
    .with_preview(|_, row: &Row| match row.preview.as_ref()? {
        Preview::File(path) => Some((path.as_path().into(), None)),
        Preview::Document(id) => Some((PathOrId::Id(*id), None)),
    })
    .with_key_action(alt!('s'), action(IndexAction::Stage))
    .with_key_action(alt!('a'), action(IndexAction::StageWhole))
    .with_key_action(alt!('u'), action(IndexAction::Unstage))
    .with_key_action(alt!('c'), |cx, _| {
        commit(cx.editor);
        true
    })
    .with_key_action(alt!('p'), |cx, _| {
        push(cx);
        true
    });
    compositor.push(Box::new(overlaid(picker)));
}

/// Stage or unstage the file of `row`; the picker reopens with the result.
/// Returns whether the picker closes.
fn index_action(
    cx: &mut compositor::Context,
    root: &Path,
    row: &Row,
    action: IndexAction,
    initial: bool,
) -> bool {
    let change = &row.change;
    let path = change.path.as_str();
    let refuse = |cx: &mut compositor::Context, message: String| {
        cx.editor.set_error(message);
        false
    };
    let args: Vec<&str> = match (action, change.part) {
        (_, Part::Unsaved) => {
            return refuse(
                cx,
                format!("{path} has unsaved edits; save it before staging (Git only sees saved files)"),
            )
        }
        (_, Part::Conflict) => {
            return refuse(
                cx,
                format!("{path} has merge conflicts; resolve them before staging"),
            )
        }
        (IndexAction::Unstage, Part::Unstaged) if !change.partial => {
            return refuse(cx, format!("{path} has no staged changes"))
        }
        (IndexAction::Unstage, Part::Untracked) => {
            return refuse(cx, format!("{path} is untracked, not staged"))
        }
        (IndexAction::Unstage, _) if initial => {
            let mut args = vec!["rm", "--cached", "--quiet", "--"];
            args.extend(change.orig.as_deref());
            args.push(path);
            args
        }
        (IndexAction::Unstage, _) => {
            let mut args = vec!["restore", "--staged", "--"];
            args.extend(change.orig.as_deref());
            args.push(path);
            args
        }
        (IndexAction::Stage | IndexAction::StageWhole, Part::Staged) if !change.partial => {
            cx.editor.set_status(format!("{path} is already staged"));
            return false;
        }
        (IndexAction::Stage, _) if change.partial => {
            return refuse(
                cx,
                format!("{path} is partly staged; Alt-a stages the whole file, including its unstaged changes"),
            )
        }
        (IndexAction::Stage | IndexAction::StageWhole, _) => vec!["add", "--", path],
    };
    let root = root.to_owned();
    if cx.editor.git.running(&root) == Some("commit") {
        return refuse(
            cx,
            "A commit is running in this repository; stage after it finishes".into(),
        );
    }
    let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
    let done = match action {
        IndexAction::Stage => format!("Staged {path}"),
        IndexAction::StageWhole => format!("Staged all of {path}"),
        IndexAction::Unstage => format!("Unstaged {path}; its changes remain in the working tree"),
    };
    let select = (change.path.clone(), change.part);
    let unsaved = unsaved_buffers(cx.editor, &root);
    cx.jobs.callback(async move {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = git(&root, &args).await;
        let loaded = load(&root, unsaved).await;
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                show(editor, compositor, root.clone(), loaded, Some(select));
                match result {
                    Ok(_) => {
                        editor.set_status(done);
                        refresh_drafts(editor, &root);
                    }
                    Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
                }
            },
        )))
    });
    true
}

// ---------------------------------------------------------------------------
// Commit drafts

struct Prepared {
    snapshot: Snapshot,
    context: String,
}

fn staged_label(code: char) -> &'static str {
    match code {
        'A' => "new file:",
        'D' => "deleted:",
        'T' => "typechange:",
        _ => "modified:",
    }
}

/// Everything a draft shows below the scissors line, derived from the index
/// tree the commit will record.
async fn prepare(
    root: &Path,
    unsaved: &[String],
    failure: Option<&str>,
    discussion: Option<&str>,
) -> anyhow::Result<Prepared> {
    let head = head(root).await?;
    let tree = git_text(root, &["write-tree"])
        .await
        .map_err(|err| anyhow!("Cannot commit: {err}"))?;
    let base = match &head {
        Some(head) => format!("{head}^{{tree}}"),
        None => String::from_utf8(
            output(
                root,
                &["hash-object", "-t", "tree", "--stdin"],
                Some(Vec::new()),
                QUICK,
            )
            .await?
            .stdout,
        )?
        .trim()
        .to_owned(),
    };
    let staged = status::parse_name_status(
        &git(
            root,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-status",
                &base,
                &tree,
            ],
        )
        .await?,
    )?;
    let diff = git(
        root,
        &[
            "diff-tree",
            "-r",
            "-p",
            "--no-renames",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &base,
            &tree,
        ],
    )
    .await?;
    let status = status(root).await?;
    let subject = match &head {
        Some(_) => git_text(root, &["log", "-1", "--no-color", "--format=%h %s", "HEAD"])
            .await
            .ok(),
        None => None,
    };

    let mut text = format!(
        "{COMMIT_SEPARATOR}\n\
# Do not modify or remove the line above; everything below it is ignored.\n\
# :w commits the message above it · :w! also commits when staged files have\n\
# unsaved edits · :bc! discards this draft.\n#\n# On branch {}\n",
        branch_line(&status)
    );
    match &subject {
        Some(subject) => text.push_str(&format!("# HEAD {}\n", safe_text(subject))),
        None => text.push_str("# No commits yet\n"),
    }
    if let Some(discussion) = discussion {
        text.push_str(&format!(
            "# Review discussion {}: after :git-push, :review-fixed replies with this commit\n",
            safe_text(discussion)
        ));
    }
    text.push_str("#\n");
    if staged.is_empty() {
        text.push_str(
            "# Nothing is staged yet: stage changes with :git-status (Alt-s), then :w.\n",
        );
    } else {
        text.push_str("# Changes to be committed:\n");
        for (code, path) in &staged {
            text.push_str(&format!("#\t{:<12}{}\n", staged_label(*code), path));
        }
    }
    let section = |title: &str, part: Part| {
        let paths: Vec<_> = status
            .changes
            .iter()
            .filter(|c| c.part == part)
            .map(|c| format!("#\t{}\n", c.path))
            .collect();
        if paths.is_empty() {
            String::new()
        } else {
            format!("#\n# {title}\n{}", paths.concat())
        }
    };
    text.push_str(&section("Changes not staged for commit:", Part::Unstaged));
    text.push_str(&section("Unmerged paths:", Part::Conflict));
    text.push_str(&section("Untracked files:", Part::Untracked));
    if !unsaved.is_empty() {
        text.push_str("#\n# Unsaved buffers (Git sees only their saved version):\n");
        for path in unsaved {
            let staged = staged.iter().any(|(_, p)| p == path);
            text.push_str(&format!(
                "#\t{path}{}\n",
                if staged { "  (staged)" } else { "" }
            ));
        }
    }
    if let Some(failure) = failure {
        text.push_str("#\n# The last commit attempt failed:\n");
        for line in failure.lines().take(60) {
            text.push_str(&format!("#   {line}\n"));
        }
    }
    text.push_str("#\n");
    let diff = String::from_utf8_lossy(&diff);
    if diff.len() > DRAFT_DIFF {
        let end = (0..=DRAFT_DIFF)
            .rev()
            .find(|&i| diff.is_char_boundary(i))
            .unwrap_or(0);
        text.push_str(&diff[..end]);
        text.push_str("\n# … diff truncated\n");
    } else {
        text.push_str(&diff);
    }
    Ok(Prepared {
        snapshot: Snapshot {
            head,
            tree,
            staged: staged.into_iter().map(|(_, path)| path).collect(),
        },
        context: text,
    })
}

fn prune(editor: &mut Editor) {
    let live: Vec<_> = editor
        .git
        .drafts
        .keys()
        .filter(|id| editor.documents.contains_key(id))
        .copied()
        .collect();
    editor.git.drafts.retain(|id, _| live.contains(id));
}

pub(crate) fn is_draft(editor: &Editor, doc: DocumentId) -> bool {
    editor.git.drafts.contains_key(&doc)
}

/// `:git-commit`: open the repository's commit draft.
pub(crate) fn commit(editor: &mut Editor) {
    prune(editor);
    let repo = match current_repo(editor) {
        Ok(repo) => repo,
        Err(err) => return editor.set_error(err),
    };
    let Some(branch) = repo.branch.clone() else {
        return editor.set_error("Detached HEAD; check out a branch to commit");
    };
    if let Some((&id, _)) = editor.git.drafts.iter().find(|(_, d)| d.root == repo.root) {
        editor.switch(id, Action::Replace);
        return editor.set_status("The commit draft for this repository is open; :w commits");
    }
    let unsaved: Vec<_> = unsaved_buffers(editor, &repo.root)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let discussion = crate::review::selected_label(editor, &repo.root, &branch);
    editor.set_status("Preparing commit draft…");
    tokio::spawn(async move {
        let prepared = prepare(&repo.root, &unsaved, None, discussion.as_deref()).await;
        job::dispatch(move |editor, _| match prepared {
            Ok(prepared) => open_draft(editor, repo.root, branch, discussion, prepared),
            Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
        })
        .await;
    });
}

fn open_draft(
    editor: &mut Editor,
    root: PathBuf,
    branch: String,
    discussion: Option<String>,
    prepared: Prepared,
) {
    if let Some((&id, _)) = editor.git.drafts.iter().find(|(_, d)| d.root == root) {
        editor.switch(id, Action::Replace);
        return;
    }
    let staged = prepared.snapshot.staged.len();
    let id = editor.new_file(Action::Replace);
    let loader = editor.syn_loader.load();
    let (view, doc) = current!(editor);
    doc.set_virtual_name(Some(format!("[git-commit] {branch}")));
    let _ = doc.set_language_by_language_id("git-commit", &loader);
    let text = format!("\n\n{}", prepared.context);
    let transaction = Transaction::change(doc.text(), [(0, 0, Some(text.into()))].into_iter())
        .with_selection(Selection::point(0));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    // The template alone is not an unsent message; typing makes it one.
    doc.reset_modified();
    editor.git.drafts.insert(
        id,
        Draft {
            root,
            branch: branch.clone(),
            snapshot: prepared.snapshot,
            committing: false,
            revision: 0,
            discussion,
        },
    );
    editor.set_status(format!(
        "Commit draft for {branch}: {staged} staged file(s) · write the message, :w commits, :bc! discards"
    ));
}

/// Replace the draft's text below the scissors line, keeping the message.
fn replace_context(editor: &mut Editor, id: DocumentId, context: &str) {
    let Some(doc) = editor.documents.get_mut(&id) else {
        return;
    };
    // Any view that shows (or showed) the draft can carry the change.
    let Some(&view_id) = doc.selections().keys().next() else {
        return;
    };
    let text = doc.text().clone();
    let separator = text
        .lines()
        .position(|line| line.to_string().trim_end() == COMMIT_SEPARATOR)
        .map(|line| text.line_to_char(line));
    let start = separator.unwrap_or(text.len_chars());
    let blank = text.slice(..start).chars().all(char::is_whitespace);
    let prefix = if separator.is_none() && !text.to_string().ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let transaction = Transaction::change(
        &text,
        [(
            start,
            text.len_chars(),
            Some(format!("{prefix}{context}").into()),
        )]
        .into_iter(),
    );
    let modified = doc.is_modified();
    doc.apply(&transaction, view_id);
    if editor.tree.contains(view_id) {
        doc.append_changes_to_history(editor.tree.get_mut(view_id));
    }
    if blank && !modified {
        doc.reset_modified();
    }
}

/// Show the current staged state in open drafts of `root` after staging.
fn refresh_drafts(editor: &mut Editor, root: &Path) {
    prune(editor);
    let unsaved: Vec<_> = unsaved_buffers(editor, root)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let drafts: Vec<_> = editor
        .git
        .drafts
        .iter()
        .filter(|(_, d)| d.root == root && !d.committing)
        .map(|(&id, d)| (id, d.revision, d.discussion.clone()))
        .collect();
    for (id, revision, discussion) in drafts {
        let root = root.to_owned();
        let unsaved = unsaved.clone();
        tokio::spawn(async move {
            let Ok(prepared) = prepare(&root, &unsaved, None, discussion.as_deref()).await else {
                return;
            };
            job::dispatch(move |editor, _| {
                if let Some(draft) = editor.git.drafts.get_mut(&id) {
                    // A commit attempt or another update came first.
                    if draft.committing || draft.revision != revision {
                        return;
                    }
                    draft.revision += 1;
                    draft.snapshot = prepared.snapshot;
                    replace_context(editor, id, &prepared.context);
                }
            })
            .await;
        });
    }
}

enum Failure {
    /// HEAD or the index changed: the draft shows the new state.
    Changed(Prepared),
    /// Git refused the commit, e.g. a hook or signing failed.
    Refused(String, Option<Prepared>),
    Other(anyhow::Error),
}

struct Committed {
    oid: String,
    subject: String,
    warnings: Vec<&'static str>,
}

async fn create_commit(
    draft: &Draft,
    message: &str,
    unsaved: &[String],
) -> Result<Committed, Failure> {
    let root = &draft.root;
    let reference = format!("refs/heads/{}", draft.branch);
    let current = git_text(root, &["symbolic-ref", "-q", "HEAD"])
        .await
        .unwrap_or_default();
    if current != reference {
        return Err(Failure::Other(anyhow!(
            "HEAD is no longer on {}; open a new draft with :git-commit on the branch to commit to",
            draft.branch
        )));
    }
    let discussion = draft.discussion.as_deref();
    let prepared = prepare(root, unsaved, None, discussion)
        .await
        .map_err(Failure::Other)?;
    if prepared.snapshot.head != draft.snapshot.head
        || prepared.snapshot.tree != draft.snapshot.tree
    {
        return Err(Failure::Changed(prepared));
    }
    if prepared.snapshot.staged.is_empty() {
        return Err(Failure::Other(anyhow!(
            "Nothing is staged; stage changes with :git-status (Alt-s), then :w"
        )));
    }
    let out = output(
        root,
        &["commit", "--quiet", "--cleanup=whitespace", "-F", "-"],
        Some(message.as_bytes().to_vec()),
        HOOKS,
    )
    .await
    .map_err(Failure::Other)?;
    if !out.success {
        let text = out.message();
        let prepared = prepare(root, unsaved, Some(&text), discussion).await.ok();
        return Err(Failure::Refused(text, prepared));
    }
    let info = git_text(
        root,
        &[
            "log",
            "-1",
            "--no-color",
            "--format=%H%x1f%T%x1f%P%x1f%s",
            &reference,
        ],
    )
    .await
    .map_err(Failure::Other)?;
    let fields: Vec<_> = info.split('\x1f').collect();
    let [oid, tree, parents, subject] = fields[..] else {
        return Err(Failure::Other(anyhow!("Cannot read the new commit")));
    };
    let mut warnings = Vec::new();
    let parents: Vec<_> = parents.split_whitespace().collect();
    if parents
        != draft
            .snapshot
            .head
            .as_deref()
            .into_iter()
            .collect::<Vec<_>>()
    {
        warnings.push("its parent is not the reviewed HEAD");
    }
    if tree != draft.snapshot.tree {
        warnings.push("a hook changed the committed content; check it with git show");
    }
    Ok(Committed {
        oid: oid.to_owned(),
        subject: safe_text(subject),
        warnings,
    })
}

/// `:w` in a commit draft.
pub(crate) fn send(editor: &mut Editor, force: bool) {
    prune(editor);
    let doc = doc!(editor);
    let (id, version) = (doc.id(), doc.version());
    let message = commit_message(&doc.text().to_string());
    let Some(draft) = editor.git.drafts.get(&id).cloned() else {
        return editor.set_error("Not a commit draft; open one with :git-commit");
    };
    if draft.committing {
        return editor.set_status("The commit is already running");
    }
    if let Some(what) = editor.git.running(&draft.root) {
        return editor.set_error(format!(
            "A {what} is running in this repository; wait for it or :git-cancel"
        ));
    }
    let message = match message {
        Ok(message) => message,
        Err(err) => return editor.set_error(err),
    };
    let unsaved: Vec<_> = unsaved_buffers(editor, &draft.root)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let blocking: Vec<_> = unsaved
        .iter()
        .filter(|path| draft.snapshot.staged.contains(path))
        .map(String::as_str)
        .collect();
    if !force && !blocking.is_empty() {
        return editor.set_error(format!(
            "Unsaved edits in {}: the commit would contain only the staged version. Save and stage them, or :w! to commit anyway",
            blocking.join(", ")
        ));
    }
    if let Some(draft) = editor.git.drafts.get_mut(&id) {
        draft.committing = true;
        draft.revision += 1;
    }
    editor.set_status(format!("Committing on {}…", draft.branch));
    let root = draft.root.clone();
    let task = tokio::spawn(async move {
        let result = create_commit(&draft, &message, &unsaved).await;
        job::dispatch(move |editor, _| finish_commit(editor, id, version, draft, result)).await;
    });
    editor.git.running.insert(
        root,
        Running {
            what: "commit",
            task: task.abort_handle(),
        },
    );
}

fn finish_commit(
    editor: &mut Editor,
    id: DocumentId,
    version: i32,
    draft: Draft,
    result: Result<Committed, Failure>,
) {
    if editor
        .git
        .running
        .get(&draft.root)
        .is_some_and(|r| r.what == "commit")
    {
        editor.git.running.remove(&draft.root);
    }
    if let Some(draft) = editor.git.drafts.get_mut(&id) {
        draft.committing = false;
    }
    let update = |editor: &mut Editor, prepared: Prepared| {
        if let Some(draft) = editor.git.drafts.get_mut(&id) {
            draft.revision += 1;
            draft.snapshot = prepared.snapshot;
            replace_context(editor, id, &prepared.context);
        }
    };
    match result {
        Ok(committed) => {
            editor.git.drafts.remove(&id);
            if editor
                .documents
                .get(&id)
                .is_some_and(|doc| doc.version() == version)
            {
                if let Some(doc) = editor.documents.get_mut(&id) {
                    doc.reset_modified();
                }
                let _ = editor.close_document(id, true);
            }
            refresh_diff_bases(editor, &draft.root, &draft.snapshot.staged);
            let warnings = if committed.warnings.is_empty() {
                String::new()
            } else {
                format!(" · note: {}", committed.warnings.join("; "))
            };
            let status = format!(
                "Committed {} {} on {} · local only, not pushed: :git-push{warnings}",
                &committed.oid[..committed.oid.len().min(10)],
                committed.subject,
                draft.branch,
            );
            // The new HEAD reloads a displayed review of this branch; keep this
            // message instead of the review summary.
            if editor.review.enabled {
                editor.review.after_load =
                    Some((draft.root.clone(), draft.branch.clone(), status.clone()));
            }
            if committed.warnings.is_empty() {
                editor.set_status(status);
            } else {
                editor.set_error(status);
            }
        }
        Err(Failure::Changed(prepared)) => {
            update(editor, prepared);
            editor.set_error(
                "HEAD or the staged changes changed since this draft was prepared; it now shows the current state. Review it and :w again",
            );
        }
        Err(Failure::Refused(text, prepared)) => {
            if let Some(prepared) = prepared {
                update(editor, prepared);
            }
            editor.set_error(format!(
                "Commit failed: {} · the message is kept; details are in the draft",
                summary(&text, 2)
            ));
        }
        Err(Failure::Other(err)) => editor.set_error(safe_text(&format!("{err:#}"))),
    }
}

/// Committed files' change markers compare with the new HEAD.
fn refresh_diff_bases(editor: &mut Editor, root: &Path, paths: &[String]) {
    let providers = editor.diff_providers.clone();
    for doc in editor.documents_mut() {
        let Some(path) = doc.path().cloned() else {
            continue;
        };
        if relative(root, &path).is_some_and(|p| paths.contains(&p)) {
            if let Some(base) = providers.get_diff_base(&path) {
                doc.set_diff_base(base);
            }
        }
    }
}

/// `:git-cancel`: stop the running commit in the focused repository.
pub(crate) fn cancel(editor: &mut Editor) {
    let repo = match current_repo(editor) {
        Ok(repo) => repo,
        Err(err) => return editor.set_error(err),
    };
    match editor.git.running(&repo.root) {
        Some(what) => {
            if let Some(running) = editor.git.running.remove(&repo.root) {
                // Git receives SIGTERM and removes its lock files.
                running.task.abort();
            }
            for draft in editor.git.drafts.values_mut() {
                if draft.root == repo.root {
                    draft.committing = false;
                }
            }
            editor.set_status(format!("Cancelled the {what}"));
        }
        None => editor.set_status("Nothing to cancel in this repository"),
    }
}

#[cfg(all(test, feature = "integration", unix))]
mod editor_tests {
    use super::*;
    use crate::{compositor::Compositor, config::Config};
    use helix_core::syntax;
    use std::{fs, sync::Arc};

    pub(crate) fn sh(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A repository on branch `topic` with one commit of `a.txt` and `b.txt`.
    pub(crate) fn fixture(commit: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap().join("repo");
        fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "--quiet", "--initial-branch=topic"]);
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join("b.txt"), "b\n").unwrap();
        if commit {
            sh(&root, &["add", "."]);
            sh(&root, &["commit", "--quiet", "-m", "base"]);
        }
        (dir, root)
    }

    pub(crate) fn editor() -> Editor {
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

    /// Apply queued job results until `done`.
    pub(crate) async fn pump(
        editor: &mut Editor,
        jobs: &mut job::Jobs,
        compositor: &mut Compositor,
        done: impl Fn(&Editor, &mut Compositor) -> bool,
    ) {
        while !done(editor, compositor) {
            let callback = tokio::time::timeout(Duration::from_secs(20), jobs.callbacks.recv())
                .await
                .unwrap_or_else(|_| panic!("no job result; status: {:?}", status_text(editor)))
                .unwrap();
            jobs.handle_callback(editor, compositor, Ok(Some(callback)));
        }
    }

    pub(crate) fn has_picker(compositor: &mut Compositor) -> bool {
        compositor
            .find::<crate::ui::overlay::Overlay<Picker<Row, View>>>()
            .is_some()
    }

    pub(crate) fn committed(editor: &Editor) -> bool {
        editor.git.drafts.values().all(|d| !d.committing)
    }

    pub(crate) fn status_text(editor: &Editor) -> String {
        editor
            .get_status()
            .map(|(text, _)| text.to_string())
            .unwrap_or_default()
    }

    #[track_caller]
    pub(crate) fn expect(editor: &Editor, text: &str) {
        let status = status_text(editor);
        assert!(status.contains(text), "expected {text:?} in {status:?}");
    }

    fn rows(loaded: &Loaded) -> Vec<String> {
        loaded
            .rows
            .iter()
            .map(|r| format!("{} | {}", r.change.describe(), r.change.path))
            .collect()
    }

    fn preview(row: &Row) -> String {
        match &row.preview {
            Some(Preview::File(path)) => fs::read_to_string(path).unwrap(),
            _ => String::new(),
        }
    }

    async fn act(
        editor: &mut Editor,
        jobs: &mut job::Jobs,
        compositor: &mut Compositor,
        root: &Path,
        (path, part): (&str, Part),
        action: IndexAction,
    ) -> bool {
        let loaded = load(root, unsaved_buffers(editor, root)).await.unwrap();
        let row = loaded
            .rows
            .iter()
            .find(|r| r.change.path == path && r.change.part == part)
            .unwrap();
        while compositor.pop().is_some() {}
        let started = {
            let mut cx = compositor::Context {
                editor,
                jobs,
                scroll: None,
            };
            index_action(&mut cx, root, row, action, false)
        };
        if started {
            pump(editor, jobs, compositor, |_, c| has_picker(c)).await;
        }
        started
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn staging_keeps_existing_index_content() {
        let (_dir, root) = fixture(true);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        fs::write(root.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
        sh(&root, &["add", "a.txt"]);
        fs::write(root.join("a.txt"), "ONE\ntwo\nTHREE\n").unwrap();
        fs::write(root.join("b.txt"), "B\n").unwrap();
        fs::write(root.join("c.txt"), "new\n").unwrap();
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();

        let loaded = load(&root, unsaved_buffers(editor, &root)).await.unwrap();
        assert_eq!(
            rows(&loaded),
            [
                "staged modified (partly staged) | a.txt",
                "unstaged modified (partly staged) | a.txt",
                "unstaged modified | b.txt",
                "untracked | c.txt",
            ]
        );
        // Each part previews only its own diff.
        assert!(preview(&loaded.rows[0]).contains("+ONE"));
        assert!(!preview(&loaded.rows[0]).contains("THREE"));
        assert!(preview(&loaded.rows[1]).contains("+THREE"));
        assert!(!preview(&loaded.rows[1]).contains("+ONE"));
        assert!(
            matches!(&loaded.rows[3].preview, Some(Preview::File(p)) if p == &root.join("c.txt"))
        );

        let cached = |root: &Path| sh(root, &["diff", "--cached", "--no-color"]);
        // Plain staging refuses a partly staged file; the index is untouched.
        assert!(
            !act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("a.txt", Part::Unstaged),
                IndexAction::Stage
            )
            .await
        );
        expect(editor, "Alt-a stages the whole file");
        assert!(cached(&root).contains("+ONE") && !cached(&root).contains("THREE"));
        // Staging another file keeps a.txt's staged part as it was.
        assert!(
            act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("b.txt", Part::Unstaged),
                IndexAction::Stage
            )
            .await
        );
        assert_eq!(status_text(editor), "Staged b.txt");
        assert_eq!(
            sh(&root, &["diff", "--cached", "--name-only"]),
            "a.txt\nb.txt"
        );
        assert!(!cached(&root).contains("THREE"));
        // New files.
        assert!(
            act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("c.txt", Part::Untracked),
                IndexAction::Stage
            )
            .await
        );
        assert!(sh(&root, &["diff", "--cached", "--name-status"]).contains("A\tc.txt"));
        // Unstaging keeps the working tree.
        assert!(
            act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("b.txt", Part::Staged),
                IndexAction::Unstage
            )
            .await
        );
        assert_eq!(
            sh(&root, &["diff", "--cached", "--name-only"]),
            "a.txt\nc.txt"
        );
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
        // Staging the whole partly staged file is its own action.
        assert!(
            act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("a.txt", Part::Unstaged),
                IndexAction::StageWhole
            )
            .await
        );
        assert_eq!(sh(&root, &["diff", "--name-only", "--", "a.txt"]), "");
        assert!(cached(&root).contains("+THREE"));

        // Unsaved buffers are listed first and cannot be staged.
        {
            let (view, doc) = current!(editor);
            let change = Transaction::change(doc.text(), [(0, 0, Some("x".into()))].into_iter());
            doc.apply(&change, view.id);
        }
        let loaded = load(&root, unsaved_buffers(editor, &root)).await.unwrap();
        assert_eq!(rows(&loaded)[0], "unsaved buffer | a.txt");
        assert!(
            !act(
                editor,
                &mut jobs,
                &mut compositor,
                &root,
                ("a.txt", Part::Unsaved),
                IndexAction::StageWhole
            )
            .await
        );
        expect(editor, "unsaved edits");
    }

    /// The picker's keys: Alt-s stages the selected file and the picker comes
    /// back with the new state; Alt-c opens the commit draft.
    #[tokio::test(flavor = "multi_thread")]
    async fn picker_keys_stage_and_open_the_draft() {
        use helix_view::input::Event;
        let (_dir, root) = fixture(true);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        fs::write(root.join("b.txt"), "B\n").unwrap();
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        // Pickers match their items while rendering, before keys act on them.
        let key =
            |editor: &mut Editor, jobs: &mut job::Jobs, compositor: &mut Compositor, event| {
                let mut cx = compositor::Context {
                    editor,
                    jobs,
                    scroll: None,
                };
                let area = helix_view::graphics::Rect::new(0, 0, 120, 40);
                compositor.render(area, &mut tui::buffer::Buffer::empty(area), &mut cx);
                compositor.handle_event(&Event::Key(event), &mut cx)
            };
        {
            let mut cx = compositor::Context {
                editor,
                jobs: &mut jobs,
                scroll: None,
            };
            status_view(&mut cx);
        }
        pump(editor, &mut jobs, &mut compositor, |_, c| has_picker(c)).await;
        expect(
            editor,
            "topic (no upstream) · 0 staged, 1 unstaged, 0 untracked",
        );
        key(editor, &mut jobs, &mut compositor, alt!('s'));
        assert!(!has_picker(&mut compositor));
        pump(editor, &mut jobs, &mut compositor, |_, c| has_picker(c)).await;
        assert_eq!(sh(&root, &["diff", "--cached", "--name-only"]), "b.txt");
        expect(editor, "Staged b.txt");
        key(editor, &mut jobs, &mut compositor, alt!('c'));
        assert!(!has_picker(&mut compositor));
        pump(editor, &mut jobs, &mut compositor, |e, _| {
            is_draft(e, doc!(e).id())
        })
        .await;
        assert!(doc!(editor)
            .text()
            .to_string()
            .contains("#\tmodified:   b.txt"));
    }

    /// Paths are named as Git names them: a tracked symlink is not resolved.
    #[test]
    fn relative_paths_keep_file_symlinks() {
        let (_dir, root) = fixture(true);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("target.txt"), root.join("link.txt"))
            .unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        assert_eq!(
            relative(&root, &root.join("link.txt")).as_deref(),
            Some("link.txt")
        );
        assert_eq!(
            relative(&root, &root.join("dir/new.txt")).as_deref(),
            Some("dir/new.txt")
        );
        assert_eq!(relative(&root, &outside.path().join("target.txt")), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unstaging_before_the_first_commit() {
        let (_dir, root) = fixture(false);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        sh(&root, &["add", "a.txt"]);
        let loaded = load(&root, vec![]).await.unwrap();
        assert_eq!(
            rows(&loaded),
            ["staged new file | a.txt", "untracked | b.txt"]
        );
        let started = {
            let mut cx = compositor::Context {
                editor,
                jobs: &mut jobs,
                scroll: None,
            };
            index_action(&mut cx, &root, &loaded.rows[0], IndexAction::Unstage, true)
        };
        assert!(started);
        pump(editor, &mut jobs, &mut compositor, |_, c| has_picker(c)).await;
        assert_eq!(sh(&root, &["diff", "--cached", "--name-only"]), "");
        assert!(root.join("a.txt").exists());
    }

    pub(crate) fn type_message(editor: &mut Editor, message: &str) {
        let (view, doc) = current!(editor);
        let change = Transaction::change(doc.text(), [(0, 0, Some(message.into()))].into_iter());
        doc.apply(&change, view.id);
    }

    /// Install or remove a pre-commit hook. A child process writes it: an
    /// executable written by this multi-threaded process could be held open by
    /// a concurrently forked child and fail with "Text file busy".
    pub(crate) fn hook(root: &Path, script: Option<&str>) {
        let path = root.join(".git/hooks/pre-commit");
        match script {
            Some(script) => {
                let status = std::process::Command::new("sh")
                    .args([
                        "-c",
                        "printf '%s\\n' \"$1\" > \"$2\" && chmod 755 \"$2\"",
                        "sh",
                    ])
                    .arg(format!("#!/bin/sh\n{script}"))
                    .arg(&path)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        }
    }

    pub(crate) async fn open(
        editor: &mut Editor,
        jobs: &mut job::Jobs,
        compositor: &mut Compositor,
    ) -> DocumentId {
        commit(editor);
        pump(editor, jobs, compositor, |e, _| is_draft(e, doc!(e).id())).await;
        let id = doc!(editor).id();
        assert!(is_draft(editor, id), "{}", status_text(editor));
        id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_draft_commits_exactly_the_reviewed_index() {
        let (_dir, root) = fixture(true);
        let base = sh(&root, &["rev-parse", "HEAD"]);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        fs::write(root.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
        sh(&root, &["add", "a.txt"]);
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        let file = doc!(editor).id();
        let draft = open(editor, &mut jobs, &mut compositor).await;
        let text = doc!(editor).text().to_string();
        assert!(
            text.starts_with(&format!("\n\n{COMMIT_SEPARATOR}\n")),
            "{text}"
        );
        assert!(text.contains("#\tmodified:   a.txt"), "{text}");
        assert!(text.contains("diff --git a/a.txt b/a.txt"), "{text}");
        assert!(!doc!(editor).is_modified());
        // Running :git-commit again returns to the open draft.
        commit(editor);
        assert_eq!(doc!(editor).id(), draft);

        send(editor, false);
        expect(editor, "Commit message is empty");
        type_message(editor, "fix: a\n\nbody");

        // Unsaved edits of a staged file block a plain :w.
        editor.switch(file, Action::Replace);
        type_message(editor, "unsaved ");
        editor.switch(draft, Action::Replace);
        send(editor, false);
        expect(editor, "Unsaved edits in a.txt");
        assert!(!editor.git.drafts[&draft].committing);
        editor.switch(file, Action::Replace);
        doc_mut!(editor).reset_modified();
        editor.switch(draft, Action::Replace);

        // The index changed after the draft was prepared: refresh, no commit.
        fs::write(root.join("b.txt"), "B\n").unwrap();
        sh(&root, &["add", "b.txt"]);
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        expect(editor, "changed since this draft");
        assert_eq!(sh(&root, &["rev-parse", "HEAD"]), base);
        let text = doc!(editor).text().to_string();
        assert!(text.starts_with("fix: a\n\nbody"), "{text}");
        assert!(text.contains("#\tmodified:   b.txt"), "{text}");

        // A failing hook keeps the draft and its message and shows the output.
        hook(&root, Some("echo 'lint failed: x' >&2; exit 1"));
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        expect(editor, "Commit failed: lint failed: x");
        let text = doc!(editor).text().to_string();
        assert!(text.starts_with("fix: a\n\nbody"), "{text}");
        assert!(text.contains("#   lint failed: x"), "{text}");
        assert_eq!(sh(&root, &["rev-parse", "HEAD"]), base);

        // A second :w while committing does not start another commit.
        hook(&root, None);
        send(editor, false);
        send(editor, false);
        assert_eq!(status_text(editor), "The commit is already running");
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        let status = status_text(editor);
        assert!(
            status.starts_with("Committed ")
                && status.contains("fix: a on topic · local only, not pushed"),
            "{status}"
        );
        assert!(editor.git.drafts.is_empty());
        assert!(!editor.documents.contains_key(&draft));
        assert_eq!(sh(&root, &["log", "-1", "--format=%B"]), "fix: a\n\nbody");
        assert_eq!(sh(&root, &["rev-parse", "HEAD^"]), base);
        assert_eq!(
            sh(&root, &["show", "--name-only", "--format=", "HEAD"]),
            "a.txt\nb.txt"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commits_follow_head_branch_and_hooks() {
        let (_dir, root) = fixture(true);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        fs::write(root.join("a.txt"), "ONE\n").unwrap();
        sh(&root, &["add", "a.txt"]);
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();

        // HEAD moved: the draft refreshes instead of committing on top.
        open(editor, &mut jobs, &mut compositor).await;
        type_message(editor, "fix: a");
        // Another commit on the branch that leaves the index alone.
        let moved = sh(
            &root,
            &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "moved"],
        );
        sh(&root, &["update-ref", "refs/heads/topic", &moved]);
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        expect(editor, "changed since this draft");
        assert_eq!(sh(&root, &["log", "-1", "--format=%s"]), "moved");

        // Another branch checked out at the same commit: refused.
        sh(&root, &["checkout", "--quiet", "-b", "other"]);
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        expect(editor, "HEAD is no longer on topic");
        sh(&root, &["checkout", "--quiet", "topic"]);

        // A hook that changes the index is reported after committing.
        hook(&root, Some("echo extra > extra.txt && git add extra.txt"));
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        let status = status_text(editor);
        assert!(
            status.contains("a hook changed the committed content"),
            "{status}"
        );
        assert_eq!(
            sh(&root, &["show", "--name-only", "--format=", "HEAD"]),
            "a.txt\nextra.txt"
        );
        hook(&root, None);

        // A long hook can be cancelled; Git removes its lock.
        fs::write(root.join("b.txt"), "B\n").unwrap();
        sh(&root, &["add", "b.txt"]);
        hook(&root, Some("sleep 30"));
        let draft = open(editor, &mut jobs, &mut compositor).await;
        type_message(editor, "slow");
        let head = sh(&root, &["rev-parse", "HEAD"]);
        send(editor, false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel(editor);
        assert_eq!(status_text(editor), "Cancelled the commit");
        assert!(!editor.git.drafts[&draft].committing);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!root.join(".git/index.lock").exists());
        assert_eq!(sh(&root, &["rev-parse", "HEAD"]), head);
        cancel(editor);
        assert_eq!(status_text(editor), "Nothing to cancel in this repository");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_commit() {
        let (_dir, root) = fixture(false);
        let mut editor_value = editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        let mut compositor = Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        sh(&root, &["add", "a.txt"]);
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        open(editor, &mut jobs, &mut compositor).await;
        let text = doc!(editor).text().to_string();
        assert!(text.contains("# No commits yet"), "{text}");
        assert!(text.contains("#\tnew file:   a.txt"), "{text}");
        type_message(editor, "initial");
        send(editor, false);
        pump(editor, &mut jobs, &mut compositor, |e, _| committed(e)).await;
        expect(editor, "Committed ");
        assert_eq!(sh(&root, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(
            sh(&root, &["show", "--name-only", "--format=", "HEAD"]),
            "a.txt"
        );
    }
}
