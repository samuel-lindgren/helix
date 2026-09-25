//! Replying to and resolving review discussions. Every write starts from an
//! explicit command and is bound to one discussion node id: a reply draft keeps
//! the discussion it was opened for, whatever the cursor or checkout does later.
use super::{
    attach_documents, commits, current_context, github, list_location, refresh_editor, synchronize,
    under_cursor,
};
use crate::{
    compositor, job,
    ui::{overlay::overlaid, Picker, PickerColumn},
};
use helix_core::{Selection, Transaction};
use helix_view::{
    editor::Action,
    review::{reply_body, safe_text, Compose, Context, Thread, REPLY_SEPARATOR},
    DocumentId, Editor, ViewId,
};
use std::sync::Arc;

/// Register that holds the suggested commit id while a reply is written.
pub(crate) const COMMIT_REGISTER: char = 'h';

#[derive(Clone)]
struct Target {
    context: Context,
    repo: String,
    number: u64,
    thread: Arc<Thread>,
}

impl Target {
    fn label(&self) -> String {
        let author = self
            .thread
            .comments
            .first()
            .map_or("[deleted]", |c| c.author.as_str());
        format!("@{author} on {}", list_location(&self.thread))
    }
}

fn prune(editor: &mut Editor) {
    let live: Vec<_> = editor
        .review
        .compose
        .keys()
        .filter(|id| editor.documents.contains_key(id))
        .copied()
        .collect();
    editor.review.compose.retain(|id, _| live.contains(id));
}

pub(crate) fn is_compose(editor: &Editor, doc: DocumentId) -> bool {
    editor.review.compose.contains_key(&doc)
}

/// The reply draft's discussion, else the discussion at the cursor or selected
/// by navigation in the loaded review.
fn target(editor: &mut Editor) -> Result<Target, String> {
    prune(editor);
    let doc = doc!(editor).id();
    if let Some(compose) = editor.review.compose.get(&doc) {
        return Ok(Target {
            context: compose.context.clone(),
            repo: compose.repo.clone(),
            number: compose.number,
            thread: compose.thread.clone(),
        });
    }
    if !editor.review.enabled {
        return Err("Reviews are not loaded; run :review-refresh first".into());
    }
    synchronize(editor);
    let (Some(review), Some(context)) =
        (editor.review.review.clone(), editor.review.context.clone())
    else {
        return Err(if editor.review.status.is_empty() {
            "Reviews are not loaded; run :review-refresh first".into()
        } else {
            editor.review.status.clone()
        });
    };
    let index = under_cursor(editor)
        .ok_or("No review discussion selected; use :review-next or :review-list")?;
    Ok(Target {
        context,
        repo: review.repo.clone(),
        number: review.number,
        thread: review.threads[index].clone(),
    })
}

/// Replace a re-fetched discussion in the loaded review. Line numbers belong to
/// a PR head, so a changed head reloads the whole review instead.
fn apply_thread(editor: &mut Editor, target: &Target, thread: Thread, head: String) {
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    if review.repo != target.repo || review.number != target.number {
        return;
    }
    if review.head != head {
        let pull = editor.review.pull.clone();
        refresh_editor(editor, pull);
        return;
    }
    let mut updated = (*review).clone();
    let thread = Arc::new(thread);
    for slot in updated.threads.iter_mut().filter(|t| t.id == thread.id) {
        *slot = thread.clone();
    }
    editor.review.review = Some(Arc::new(updated));
    for compose in editor.review.compose.values_mut() {
        if compose.thread.id == thread.id {
            compose.thread = thread.clone();
        }
    }
    // Remap blocks from the new data; older mapping results are rejected.
    editor.review.generation = editor.review.generation.wrapping_add(1);
    for doc in editor.documents_mut() {
        doc.review.stamp = None;
    }
    attach_documents(editor);
}

struct Posted {
    url: String,
    resolved: Option<bool>,
    refreshed: Option<(Thread, String)>,
}

/// Refuse to post commit ids GitHub cannot link yet, unless forced.
async fn check_pushed(target: &Target, body: &str) -> anyhow::Result<()> {
    if !commits::mentions_commit(body) {
        return Ok(());
    }
    let pull = github::pull_head(&target.context, &target.repo, target.number).await?;
    let missing = commits::unpushed_references(&target.context, body, &pull).await?;
    anyhow::ensure!(
        missing.is_empty(),
        "{} not on {} (PR head {}) yet; push first, or send anyway with :w! / --force",
        missing.join(", "),
        pull.name,
        &pull.oid[..pull.oid.len().min(10)]
    );
    Ok(())
}

async fn post(target: Target, body: String, force: bool, resolve: bool) -> anyhow::Result<Posted> {
    if !force {
        check_pushed(&target, &body).await?;
    }
    let url = github::reply(&target.context, &target.thread.id, &body).await?;
    let resolved = if resolve {
        Some(
            github::set_resolved(&target.context, &target.thread.id, true)
                .await
                .map_err(|err| anyhow::anyhow!("Reply posted ({url}), but: {err}"))?,
        )
    } else {
        None
    };
    let refreshed = github::fetch_thread(&target.context, &target.thread.id)
        .await
        .ok();
    Ok(Posted {
        url,
        resolved,
        refreshed,
    })
}

/// Post `body` in the background. `draft` is the reply buffer and its version
/// at send time; it closes only if unchanged since.
fn spawn_post(
    editor: &mut Editor,
    target: Target,
    body: String,
    force: bool,
    resolve: bool,
    draft: Option<(DocumentId, i32)>,
) {
    if !target.thread.can_reply {
        editor.set_error(format!(
            "GitHub does not allow you to reply to {} (locked, or no access)",
            target.label()
        ));
        return;
    }
    editor.set_status(format!("Sending reply to {}…", target.label()));
    tokio::spawn(async move {
        let result = post(target.clone(), body, force, resolve).await;
        job::dispatch(move |editor, _| {
            if let Some((doc, _)) = draft {
                if let Some(compose) = editor.review.compose.get_mut(&doc) {
                    compose.sending = false;
                }
            }
            let posted = match result {
                Ok(posted) => posted,
                Err(err) => {
                    editor.set_error(safe_text(&format!("{err:#}")));
                    return;
                }
            };
            if let Some((doc, version)) = draft {
                editor.review.compose.remove(&doc);
                if editor
                    .documents
                    .get(&doc)
                    .is_some_and(|d| d.version() == version)
                {
                    if let Some(d) = editor.documents.get_mut(&doc) {
                        d.reset_modified();
                    }
                    let _ = editor.close_document(doc, true);
                }
            }
            if let Some((thread, head)) = posted.refreshed {
                apply_thread(editor, &target, thread, head);
            }
            editor.set_status(format!(
                "Replied to {}{} · {}",
                target.label(),
                match posted.resolved {
                    Some(true) => " and resolved it",
                    Some(false) => " (still unresolved)",
                    None => "",
                },
                posted.url
            ));
        })
        .await;
    });
}

/// `:review-reply [text]`: post `text` directly, or open a reply draft.
pub(crate) fn reply(editor: &mut Editor, text: Option<String>, force: bool, resolve: bool) {
    let target = match target(editor) {
        Ok(target) => target,
        Err(err) => return editor.set_error(err),
    };
    match text.filter(|t| !t.trim().is_empty()) {
        Some(text) => spawn_post(editor, target, text, force, resolve, None),
        None => compose(editor, target, None),
    }
}

/// `:review-fixed [rev]`: a reply draft prefilled with "Fixed in <commit>."
pub(crate) fn fixed(editor: &mut Editor, rev: Option<String>) {
    match target(editor) {
        Ok(target) => compose(editor, target, Some(rev)),
        Err(err) => editor.set_error(err),
    }
}

fn compose(editor: &mut Editor, target: Target, fixed: Option<Option<String>>) {
    if let Some((&doc, _)) = editor
        .review
        .compose
        .iter()
        .find(|(_, c)| c.thread.id == target.thread.id)
    {
        editor.switch(doc, Action::Replace);
        editor.set_status(format!(
            "Reply draft to {} is already open; :w sends it",
            target.label()
        ));
        return;
    }
    if !target.thread.can_reply {
        editor.set_error(format!(
            "GitHub does not allow you to reply to {} (locked, or no access)",
            target.label()
        ));
        return;
    }
    tokio::spawn(async move {
        let suggestion = match fixed.clone().flatten() {
            Some(rev) => commits::resolve(&target.context, &rev).await,
            None => commits::suggest(&target.context, Some(&target.thread)).await,
        };
        job::dispatch(move |editor, _| match suggestion {
            Err(err) if fixed.as_ref().is_some_and(Option::is_some) => {
                editor.set_error(safe_text(&format!("{err:#}")))
            }
            suggestion => open_draft(editor, target, suggestion.ok(), fixed.is_some()),
        })
        .await;
    });
}

fn touch_note(commit: &commits::Commit) -> &'static str {
    match commit.touch {
        commits::Touch::Lines => "last change to the discussed lines",
        commits::Touch::File => "last change to the file since the discussion",
        commits::Touch::None => "HEAD",
    }
}

fn draft_text(target: &Target, suggestion: Option<&commits::Commit>, body: &str) -> String {
    let thread = &target.thread;
    let mut text = format!(
        "{body}\n\n{REPLY_SEPARATOR}\nReply to {} · {} · {}#{}\n:w or :review-send posts the text above; :review-send --resolve also resolves; :bc! discards.\n",
        target.label(),
        if thread.resolved { "resolved" } else { "open" },
        target.repo,
        target.number,
    );
    if let Some(commit) = suggestion {
        text.push_str(&format!(
            "Suggested commit ({}): {} · paste with \"{COMMIT_REGISTER}p, or Ctrl-r {COMMIT_REGISTER} in insert mode; :review-insert-commit lists others\n",
            touch_note(commit),
            commit.describe(),
        ));
    }
    for comment in &thread.comments {
        text.push_str(&format!("\n> @{}\n", comment.author));
        for line in comment.body.lines() {
            text.push_str(&format!(
                ">{}{line}\n",
                if line.is_empty() { "" } else { " " }
            ));
        }
    }
    text
}

fn open_draft(
    editor: &mut Editor,
    target: Target,
    suggestion: Option<commits::Commit>,
    fixed: bool,
) {
    let body = match (&suggestion, fixed) {
        (Some(commit), true) => format!("Fixed in {}.", commit.sha),
        _ => String::new(),
    };
    let text = draft_text(&target, suggestion.as_ref(), &body);
    let id = editor.new_file(Action::Replace);
    let loader = editor.syn_loader.load();
    let (view, doc) = current!(editor);
    doc.set_virtual_name(Some(format!("[review-reply] {}", target.label())));
    doc.set_soft_wrap_override(Some(true));
    let _ = doc.set_language_by_language_id("markdown", &loader);
    let transaction = Transaction::change(doc.text(), [(0, 0, Some(text.into()))].into_iter())
        .with_selection(Selection::point(body.chars().count()));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    if body.is_empty() {
        // The template alone is not an unsent reply; typing makes it one.
        doc.reset_modified();
    }
    editor.review.compose.insert(
        id,
        Compose {
            context: target.context.clone(),
            repo: target.repo.clone(),
            number: target.number,
            thread: target.thread.clone(),
            sending: false,
        },
    );
    let hint = match &suggestion {
        Some(commit) => {
            let _ = editor
                .registers
                .write(COMMIT_REGISTER, vec![commit.sha.clone()]);
            format!(" · suggested {} (\"{COMMIT_REGISTER}p)", commit.describe())
        }
        None => String::new(),
    };
    editor.set_status(format!(
        "Replying to {}{hint} · :w sends, :bc! discards",
        target.label()
    ));
}

/// `:review-send` and `:write` in a reply draft.
pub(crate) fn send(editor: &mut Editor, force: bool, resolve: bool) {
    prune(editor);
    let doc = doc!(editor);
    let (id, version) = (doc.id(), doc.version());
    let body = reply_body(&doc.text().to_string());
    let Some(compose) = editor.review.compose.get_mut(&id) else {
        return editor.set_error("Not a review reply buffer; open one with :review-reply");
    };
    if compose.sending {
        return editor.set_status("Reply is already being sent");
    }
    let body = match body {
        Ok(body) => body,
        Err(err) => return editor.set_error(err),
    };
    // spawn_post refuses discussions GitHub reports as not replyable.
    compose.sending = compose.thread.can_reply;
    let target = Target {
        context: compose.context.clone(),
        repo: compose.repo.clone(),
        number: compose.number,
        thread: compose.thread.clone(),
    };
    spawn_post(editor, target, body, force, resolve, Some((id, version)));
}

/// `:review-resolve` / `:review-unresolve`.
pub(crate) fn resolve(editor: &mut Editor, resolved: bool) {
    let target = match target(editor) {
        Ok(target) => target,
        Err(err) => return editor.set_error(err),
    };
    let thread = &target.thread;
    if thread.resolved == resolved {
        return editor.set_status(format!(
            "{} is already {}",
            target.label(),
            if resolved { "resolved" } else { "open" }
        ));
    }
    if !(if resolved {
        thread.can_resolve
    } else {
        thread.can_unresolve
    }) {
        return editor.set_error(format!(
            "GitHub does not allow you to {} {} (needs write access, or the PR author's permission)",
            if resolved { "resolve" } else { "unresolve" },
            target.label()
        ));
    }
    editor.set_status(format!(
        "{} {}…",
        if resolved { "Resolving" } else { "Unresolving" },
        target.label()
    ));
    tokio::spawn(async move {
        let result = async {
            let state = github::set_resolved(&target.context, &target.thread.id, resolved).await?;
            let refreshed = github::fetch_thread(&target.context, &target.thread.id)
                .await
                .ok();
            anyhow::Ok((state, refreshed))
        }
        .await;
        job::dispatch(move |editor, _| match result {
            Ok((state, refreshed)) => {
                if let Some((thread, head)) = refreshed {
                    apply_thread(editor, &target, thread, head);
                }
                editor.set_status(format!(
                    "{} is now {}",
                    target.label(),
                    if state { "resolved" } else { "open" }
                ));
            }
            Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
        })
        .await;
    });
}

/// Optional discussion and repository for commit commands; commit lists also
/// work without a loaded review.
fn commit_scope(editor: &mut Editor) -> Option<(Context, Option<Target>)> {
    let doc = doc!(editor).id();
    let loaded = editor.review.compose.contains_key(&doc) || editor.review.enabled;
    match loaded.then(|| target(editor).ok()).flatten() {
        Some(target) => Some((target.context.clone(), Some(target))),
        None => current_context(editor).map(|context| (context, None)),
    }
}

/// Insert `text` at every cursor of `doc` in `view`, if both still exist.
fn insert_at_cursors(editor: &mut Editor, doc: DocumentId, view: ViewId, text: &str) {
    if !editor.tree.contains(view) || editor.tree.get(view).doc != doc {
        return editor.set_error("The buffer changed; commit not inserted");
    }
    let Some(document) = editor.documents.get_mut(&doc) else {
        return;
    };
    let contents = document.text().clone();
    let selection = document.selection(view).clone();
    let transaction = Transaction::change_by_selection(&contents, &selection, |range| {
        let pos = range.cursor(contents.slice(..));
        (pos, pos, Some(text.into()))
    });
    document.apply(&transaction, view);
    let view = editor.tree.get_mut(view);
    document.append_changes_to_history(view);
}

/// `:review-insert-commit`: pick a recent branch commit and insert its id.
pub(crate) fn insert_commit(cx: &mut compositor::Context) {
    let Some((context, target)) = commit_scope(cx.editor) else {
        return cx.editor.set_error("Not inside a Git branch checkout");
    };
    let (view, doc) = current_ref!(cx.editor);
    let (view, doc) = (view.id, doc.id());
    cx.jobs.callback(async move {
        let pull = match &target {
            Some(t) => github::pull_head(&t.context, &t.repo, t.number).await.ok(),
            None => None,
        };
        let entries =
            commits::list(&context, target.as_ref().map(|t| &*t.thread), pull.as_ref()).await?;
        anyhow::ensure!(!entries.is_empty(), "No commits to insert");
        Ok(job::Callback::EditorCompositor(Box::new(
            move |_, compositor| {
                let picker = Picker::new(
                    [
                        PickerColumn::new("subject", |c: &commits::Commit, _| {
                            c.subject.as_str().into()
                        }),
                        PickerColumn::new("commit", |c: &commits::Commit, _| c.short().into()),
                        PickerColumn::new("date", |c: &commits::Commit, _| c.date.as_str().into()),
                        PickerColumn::new("pr", |c: &commits::Commit, _| {
                            match c.pushed {
                                Some(true) => "pushed",
                                Some(false) => "local",
                                None => "?",
                            }
                            .into()
                        }),
                        PickerColumn::new("touches", |c: &commits::Commit, _| {
                            match c.touch {
                                commits::Touch::Lines => "lines",
                                commits::Touch::File => "file",
                                commits::Touch::None => "",
                            }
                            .into()
                        }),
                    ],
                    0,
                    entries,
                    (),
                    move |cx, commit, _| insert_at_cursors(cx.editor, doc, view, &commit.sha),
                );
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

/// `:review-yank-commit`: the suggested commit into the commit register and
/// the system clipboard.
pub(crate) fn yank_commit(editor: &mut Editor) {
    let Some((context, target)) = commit_scope(editor) else {
        return editor.set_error("Not inside a Git branch checkout");
    };
    tokio::spawn(async move {
        let result = commits::suggest(&context, target.as_ref().map(|t| &*t.thread)).await;
        job::dispatch(move |editor, _| match result {
            Ok(commit) => {
                let _ = editor
                    .registers
                    .write(COMMIT_REGISTER, vec![commit.sha.clone()]);
                let clipboard = editor
                    .registers
                    .write('+', vec![commit.sha.clone()])
                    .is_ok();
                editor.set_status(format!(
                    "Yanked {} ({}) to register {COMMIT_REGISTER}{}",
                    commit.describe(),
                    touch_note(&commit),
                    if clipboard { " and the clipboard" } else { "" }
                ));
            }
            Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
        })
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::review::Comment;

    #[test]
    fn draft_context_is_never_part_of_the_body() {
        let target = Target {
            context: Context {
                root: "/r".into(),
                git_dir: "/r/.git".into(),
                branch: "topic".into(),
                head: "abc".into(),
                config: vec![],
            },
            repo: "o/r".into(),
            number: 5,
            thread: Arc::new(Thread {
                id: "T".into(),
                path: "a.rs".into(),
                lines: Some(12..13),
                comments: vec![Comment {
                    author: "alice".into(),
                    body: "Handle empty input?\n\nThanks".into(),
                }],
                ..Thread::default()
            }),
        };
        let commit = commits::Commit {
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            subject: "fix: empty input".into(),
            date: "2026-09-24".into(),
            touch: commits::Touch::Lines,
            pushed: None,
        };
        let text = draft_text(&target, Some(&commit), "");
        assert!(text.contains("Reply to @alice on a.rs:12 · open · o/r#5"));
        assert!(text.contains("0123456789 fix: empty input"));
        assert!(text.contains("> Handle empty input?\n>\n> Thanks\n"));
        assert_eq!(
            reply_body(&text).unwrap_err(),
            "Reply is empty; write it above the separator line"
        );
        let written = format!("Fixed in {}.\nAlso added a test.\n{text}", commit.sha);
        assert_eq!(
            reply_body(&written).unwrap(),
            format!("Fixed in {}.\nAlso added a test.", commit.sha)
        );
        let prefilled = draft_text(&target, Some(&commit), "Fixed in x.");
        assert_eq!(reply_body(&prefilled).unwrap(), "Fixed in x.");
        assert!(reply_body("text without separator").is_err());
    }
}

#[cfg(all(test, unix, feature = "integration"))]
mod editor_tests {
    use super::*;
    use crate::review::{context_at, editor_tests::fixture_editor, select};
    use helix_view::review::{Comment, Review};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Handle job callbacks until `done` holds.
    async fn pump(editor: &mut Editor, jobs: &mut job::Jobs, done: impl Fn(&Editor) -> bool) {
        let mut compositor =
            crate::compositor::Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40));
        while !done(editor) {
            let callback =
                tokio::time::timeout(std::time::Duration::from_secs(10), jobs.callbacks.recv())
                    .await
                    .expect("callback")
                    .unwrap();
            jobs.handle_callback(editor, &mut compositor, Ok(Some(callback)));
        }
    }

    fn requests(log: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn type_reply(editor: &mut Editor, text: &str) {
        let (view, doc) = current!(editor);
        let change = Transaction::change(doc.text(), [(0, 0, Some(text.into()))].into_iter());
        doc.apply(&change, view.id);
    }

    /// Draft → suggestion in register h → unpushed refusal → send → the draft
    /// closes and the discussion is replaced in place; then resolve.
    #[tokio::test(flavor = "multi_thread")]
    async fn draft_suggests_commit_refuses_unpushed_and_posts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=topic"]);
        let source: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        fs::write(root.join("a.txt"), &source).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "reviewed"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        fs::write(
            root.join("a.txt"),
            source.replace("line 4\n", "line 4 fixed\n"),
        )
        .unwrap();
        git(&root, &["commit", "--quiet", "-am", "fix: line 4"]);
        let fix = git(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("b.txt"), "local\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "not pushed"]);
        let local = git(&root, &["rev-parse", "HEAD"]);

        // The PR head on GitHub is `fix`; `local` was never pushed.
        let log = dir.path().join("requests");
        let gh = dir.path().join("fake-gh");
        let out =
            |v: serde_json::Value| format!("printf '%s' '{}'", serde_json::json!({ "data": v }));
        let comment = |author: &str, body: &str| serde_json::json!({"author":{"login":author},"body":body,"diffHunk":"","url":"u","originalCommit":{"oid":reviewed}});
        let thread = |comments: Vec<serde_json::Value>, resolved: bool| {
            serde_json::json!({"node":{"pullRequest":{"headRefOid":fix},"id":"T1","path":"a.txt","line":4,"diffSide":"RIGHT","isOutdated":false,"isResolved":resolved,
            "viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":true,"originalLine":4,
            "comments":{"nodes":comments,"pageInfo":{"hasNextPage":false,"endCursor":null}}}})
        };
        let replied = vec![comment("alice", "check line 4"), comment("me", "reply")];
        fs::write(
            &gh,
            format!(
                "#!/bin/sh\ninput=$(cat)\nprintf '%s\\n' \"$input\" >> '{}'\ncase \"$input\" in\n*addPullRequestReviewThreadReply*) {};;\n*resolveReviewThread*) {};;\n*headRefName*) {};;\n*PullRequestReviewThread*) if grep -q resolveReviewThread '{}'; then {}; else {}; fi;;\n*) exit 3;;\nesac\n",
                log.display(),
                out(serde_json::json!({"addPullRequestReviewThreadReply":{"comment":{"url":"https://github.com/o/r/pull/1#discussion_r2"}}})),
                out(serde_json::json!({"resolveReviewThread":{"thread":{"isResolved":true}}})),
                out(serde_json::json!({"repository":{"pullRequest":{"headRefOid":fix,"headRefName":"topic","baseRefOid":reviewed,"headRepository":{"nameWithOwner":"me/fork"}}}})),
                log.display(),
                out(thread(replied.clone(), true)),
                out(thread(replied, false)),
            ),
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        *github::TEST_GH.lock().unwrap() = Some(gh.to_str().unwrap().to_owned());

        let mut editor_value = fixture_editor();
        let editor = &mut editor_value;
        let mut jobs = job::Jobs::new();
        editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        let file = doc!(editor).id();
        editor.review.enabled = true;
        editor.review.context = context_at(&root);
        editor.review.status = "fixture".into();
        editor.review.review = Some(Arc::new(Review {
            label: "o/r#1".into(),
            repo: "o/r".into(),
            number: 1,
            head: fix.clone(),
            threads: vec![Arc::new(Thread {
                id: "T1".into(),
                path: "a.txt".into(),
                lines: Some(4..5),
                comments: vec![Comment {
                    author: "alice".into(),
                    body: "check line 4".into(),
                }],
                commit: Some(reviewed.clone()),
                original_lines: Some(4..5),
                can_reply: true,
                can_resolve: true,
                ..Thread::default()
            })],
            sources: [(
                "a.txt".to_owned(),
                source.replace("line 4\n", "line 4 fixed\n"),
            )]
            .into(),
            originals: Default::default(),
        }));
        select(editor, 0);
        pump(editor, &mut jobs, |e| e.review.pending_selection.is_none()).await;

        reply(editor, None, false, false);
        pump(editor, &mut jobs, |e| !e.review.compose.is_empty()).await;
        let draft = doc!(editor).id();
        assert_ne!(draft, file);
        assert!(doc!(editor).path().is_none());
        assert!(!doc!(editor).is_modified());
        let text = doc!(editor).text().to_string();
        assert!(
            text.starts_with(&format!(
                "\n\n{REPLY_SEPARATOR}\nReply to @alice on a.txt:4"
            )),
            "{text}"
        );
        assert!(text.contains("fix: line 4"), "{text}");
        let register: Vec<_> = editor
            .registers
            .read(COMMIT_REGISTER, editor)
            .unwrap()
            .map(|v| v.into_owned())
            .collect();
        assert_eq!(register, vec![fix.clone()]);
        // A second :review-reply reuses the open draft.
        reply(editor, None, false, false);
        assert_eq!(editor.review.compose.len(), 1);

        // Referencing the unpushed commit is refused; nothing is posted.
        type_reply(editor, &format!("Fixed in {}", &local[..10]));
        send(editor, false, false);
        pump(editor, &mut jobs, |e| !e.review.compose[&draft].sending).await;
        let message = editor.get_status().unwrap().0.to_string();
        assert!(message.contains("not on me/fork:topic"), "{message}");
        assert!(message.contains(&local[..10]), "{message}");
        assert!(editor.documents.contains_key(&draft));
        assert!(!requests(&log)
            .iter()
            .any(|r| r["query"].as_str().unwrap().contains("mutation")));

        // Replace the reference with the pushed commit and send.
        {
            let (view, doc) = current!(editor);
            let change = Transaction::change(
                doc.text(),
                [(0, 19, Some(format!("Fixed in {fix}.").into()))].into_iter(),
            );
            doc.apply(&change, view.id);
        }
        send(editor, false, false);
        pump(editor, &mut jobs, |e| e.review.compose.is_empty()).await;
        assert!(!editor.documents.contains_key(&draft));
        let posted: Vec<_> = requests(&log)
            .into_iter()
            .filter(|r| {
                r["query"]
                    .as_str()
                    .unwrap()
                    .contains("addPullRequestReviewThreadReply")
            })
            .collect();
        assert_eq!(posted.len(), 1);
        assert_eq!(
            posted[0]["variables"],
            serde_json::json!({"id":"T1","body":format!("Fixed in {fix}.")})
        );
        let review = editor.review.review.clone().unwrap();
        assert_eq!(review.threads[0].comments.len(), 2);
        assert!(!review.threads[0].resolved);
        assert_eq!(editor.review.indicator().unwrap(), "r#1 1/1 open");

        // Resolve the discussion at the cursor.
        editor.switch(file, Action::Replace);
        resolve(editor, true);
        pump(editor, &mut jobs, |e| {
            e.review
                .review
                .as_ref()
                .is_some_and(|r| r.threads[0].resolved)
        })
        .await;
        assert_eq!(editor.review.indicator().unwrap(), "r#1 0/1 open");
        assert!(editor.get_status().unwrap().0.contains("is now resolved"));
    }
}
