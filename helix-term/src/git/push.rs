//! Pushing the checked-out branch. `:git-push` shows the destination and the
//! outgoing commits in a picker, and `Enter` pushes exactly the commit shown,
//! never forced. A rejection is reported; Helix does not fetch, merge or rebase.
use super::{current_repo, git, git_text, head, output, summary};
use crate::{
    compositor, job,
    ui::{
        overlay::{overlaid, Overlay},
        Picker, PickerColumn,
    },
};
use anyhow::{anyhow, bail};
use helix_view::{git::Running, review::safe_text, Editor};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Pushes may run hooks and talk to slow remotes.
const PUSH: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Destination {
    pub remote: String,
    /// Branch on the remote.
    pub branch: String,
    /// Make this destination the branch's upstream after pushing.
    pub set_upstream: bool,
    pub note: &'static str,
}

/// Where the branch can be pushed, the default first. `push` is Git's
/// `@{push}` and `upstream` the configured upstream, as (remote, branch).
/// Without either, every remote is offered as a new upstream; `head_remote`
/// (the PR's head repository), `push_default` and `origin` come first.
pub(crate) fn destinations(
    branch: &str,
    remotes: &[String],
    upstream: Option<(String, String)>,
    push: Option<(String, String)>,
    head_remote: Option<&str>,
    push_default: Option<&str>,
) -> Vec<Destination> {
    if let Some((remote, target)) = push {
        let note = if upstream.as_ref() == Some(&(remote.clone(), target.clone())) {
            "upstream"
        } else {
            "configured push destination"
        };
        return vec![Destination {
            remote,
            branch: target,
            set_upstream: false,
            note,
        }];
    }
    if let Some((remote, target)) = upstream {
        // Git cannot decide (e.g. `push.default=simple` with a differently named
        // upstream, or no remote-tracking branch yet): offer the candidates.
        let mut choices = vec![Destination {
            remote: remote.clone(),
            branch: target.clone(),
            set_upstream: false,
            note: "upstream",
        }];
        if target != branch {
            choices.push(Destination {
                remote,
                branch: branch.to_owned(),
                set_upstream: false,
                note: "same name as the local branch",
            });
        }
        return choices;
    }
    let rank = |remote: &str| {
        if Some(remote) == head_remote {
            0
        } else if Some(remote) == push_default {
            1
        } else if remote == "origin" {
            2
        } else {
            3
        }
    };
    let mut remotes: Vec<_> = remotes.iter().collect();
    remotes.sort_by_key(|remote| (rank(remote), remote.as_str()));
    remotes
        .into_iter()
        .map(|remote| Destination {
            remote: remote.clone(),
            branch: branch.to_owned(),
            set_upstream: true,
            note: if Some(remote.as_str()) == head_remote {
                "PR head repository · new branch, sets upstream"
            } else {
                "new branch, sets upstream"
            },
        })
        .collect()
}

/// A rejected or failed push, explained. Never suggests forcing.
fn rejection(stdout: &str, message: &str, destination: &str) -> String {
    if stdout.contains("(non-fast-forward)") || stdout.contains("(fetch first)") {
        format!(
            "Push rejected: {destination} has commits that are not in your branch. \
Integrate them outside Helix (for example git pull --rebase), then push again; Helix never force-pushes"
        )
    } else if let Some(reason) = stdout
        .lines()
        .find_map(|line| line.split_once("[remote rejected]").map(|(_, r)| r.trim()))
    {
        format!("Push rejected by the remote: {}", safe_text(reason))
    } else {
        format!("Push failed: {}", summary(message, 3))
    }
}

struct Target {
    destination: Destination,
    /// `2 to push`, `up to date`, …
    commits: String,
    /// Outgoing commits for the preview.
    log: Option<PathBuf>,
}

struct Plan {
    root: PathBuf,
    branch: String,
    oid: String,
    subject: String,
    targets: Vec<Target>,
    _logs: tempfile::TempDir,
}

async fn config(root: &Path, key: &str) -> Option<String> {
    git_text(root, &["config", "--get", key])
        .await
        .ok()
        .filter(|v| !v.is_empty())
}

async fn count(root: &Path, args: &[&str]) -> Option<u32> {
    git_text(root, args).await.ok()?.parse().ok()
}

async fn plan(root: PathBuf, branch: String, head_repo: Option<String>) -> anyhow::Result<Plan> {
    let oid = head(&root)
        .await?
        .ok_or_else(|| anyhow!("{branch} has no commits to push"))?;
    // Names starting with '-' could be taken for options.
    let remotes: Vec<String> = git_text(&root, &["remote"])
        .await?
        .lines()
        .filter(|r| !r.is_empty() && !r.starts_with('-'))
        .map(str::to_owned)
        .collect();
    if remotes.is_empty() {
        bail!("No Git remote is configured; add one with git remote add");
    }
    let upstream = match (
        config(&root, &format!("branch.{branch}.remote")).await,
        config(&root, &format!("branch.{branch}.merge")).await,
    ) {
        (Some(remote), Some(merge)) if remotes.contains(&remote) => merge
            .strip_prefix("refs/heads/")
            .map(|target| (remote, target.to_owned())),
        _ => None,
    };
    let push = output(
        &root,
        &["rev-parse", "--symbolic-full-name", "@{push}"],
        None,
        super::QUICK,
    )
    .await
    .ok()
    .filter(|out| out.success)
    .and_then(|out| {
        let reference = String::from_utf8(out.stdout).ok()?;
        let reference = reference.trim().strip_prefix("refs/remotes/")?.to_owned();
        let remote = remotes
            .iter()
            .filter(|r| reference.starts_with(&format!("{r}/")))
            .max_by_key(|r| r.len())?;
        Some((remote.clone(), reference[remote.len() + 1..].to_owned()))
    });
    let mut head_remote = None;
    if let Some(head_repo) = &head_repo {
        for remote in &remotes {
            let url = git_text(&root, &["remote", "get-url", "--push", remote]).await;
            if url
                .ok()
                .and_then(|u| crate::review::github::repository(&u))
                .as_ref()
                == Some(head_repo)
            {
                head_remote = Some(remote.clone());
                break;
            }
        }
    }
    let push_default = config(&root, "remote.pushDefault").await;
    let subject = git_text(&root, &["log", "-1", "--no-color", "--format=%h %s", &oid])
        .await
        .unwrap_or_default();
    let logs = tempfile::Builder::new().prefix("helix-push-").tempdir()?;
    let mut targets = Vec::new();
    for (i, destination) in destinations(
        &branch,
        &remotes,
        upstream,
        push,
        head_remote.as_deref(),
        push_default.as_deref(),
    )
    .into_iter()
    .enumerate()
    {
        let tracking = format!("refs/remotes/{}/{}", destination.remote, destination.branch);
        let known = git(&root, &["rev-parse", "--verify", "-q", &tracking])
            .await
            .is_ok();
        let (range, ahead, behind) = if known {
            let range = format!("{tracking}..{oid}");
            let ahead = count(&root, &["rev-list", "--count", &range]).await;
            let behind = count(
                &root,
                &["rev-list", "--count", &format!("{oid}..{tracking}")],
            )
            .await;
            (vec![range], ahead, behind)
        } else {
            let not = format!("--remotes={}", destination.remote);
            let mut args = vec!["rev-list", "--count", &oid, "--not", &not];
            let ahead = count(&root, &args).await;
            args.drain(..2);
            (args.iter().map(|s| s.to_string()).collect(), ahead, None)
        };
        let mut commits = match ahead {
            Some(0) => "up to date".to_owned(),
            Some(n) => format!("{n} to push"),
            None => "?".to_owned(),
        };
        if let Some(behind) = behind.filter(|&n| n > 0) {
            commits.push_str(&format!(", remote has {behind} more: will be rejected"));
        }
        let mut args = vec!["log", "--oneline", "--no-decorate", "--no-color", "-100"];
        args.extend(range.iter().map(String::as_str));
        let log = match git_text(&root, &args).await {
            Ok(text) if !text.is_empty() => {
                let file = logs.path().join(format!("commits-{i}"));
                std::fs::write(&file, safe_text(&text)).ok().map(|_| file)
            }
            _ => None,
        };
        targets.push(Target {
            destination,
            commits,
            log,
        });
    }
    Ok(Plan {
        root,
        branch,
        oid,
        subject: safe_text(&subject),
        targets,
        _logs: logs,
    })
}

/// `:git-push`: choose the destination of the checked-out branch and push.
pub(crate) fn push(cx: &mut compositor::Context) {
    let repo = match current_repo(cx.editor) {
        Ok(repo) => repo,
        Err(err) => return cx.editor.set_error(err),
    };
    let Some(branch) = repo.branch else {
        return cx
            .editor
            .set_error("Detached HEAD; check out a branch to push");
    };
    if let Some(what) = cx.editor.git.running(&repo.root) {
        return cx.editor.set_error(format!(
            "A {what} is running in this repository; wait for it or :git-cancel"
        ));
    }
    let head_repo = cx
        .editor
        .review
        .review
        .as_ref()
        .filter(|_| {
            cx.editor
                .review
                .context
                .as_ref()
                .is_some_and(|c| c.root == repo.root && c.branch == branch)
        })
        .and_then(|r| r.head_repo.clone());
    cx.editor.set_status("Preparing push…");
    let root = repo.root;
    cx.jobs.callback(async move {
        let plan = plan(root, branch, head_repo).await;
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| match plan {
                Ok(plan) => show(editor, compositor, plan),
                Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
            },
        )))
    });
}

fn show(editor: &mut Editor, compositor: &mut compositor::Compositor, mut plan: Plan) {
    if current_repo(editor).map(|r| r.root).as_ref() != Ok(&plan.root) {
        return editor.set_error("The focused repository changed; run :git-push again");
    }
    editor.set_status(format!(
        "Push {} at {} · Enter pushes to the selected destination, Esc cancels · never forced",
        plan.branch, plan.subject
    ));
    let targets = std::mem::take(&mut plan.targets);
    // The plan (and its commit logs) lives as long as the picker.
    let plan = Arc::new(plan);
    let columns = [
        PickerColumn::new("destination", |t: &Target, _: &()| {
            format!("{}/{}", t.destination.remote, t.destination.branch).into()
        }),
        PickerColumn::new("commits", |t: &Target, _: &()| t.commits.as_str().into()),
        PickerColumn::new("note", |t: &Target, _: &()| t.destination.note.into()),
    ];
    let picker = Picker::new(columns, 0, targets, (), move |cx, target: &Target, _| {
        start(cx.editor, &plan, target.destination.clone())
    })
    .with_preview(|_, target: &Target| Some((target.log.as_deref()?.into(), None)));
    // A repeated :git-push replaces an open push picker instead of stacking.
    compositor.remove_type::<Overlay<Picker<Target, ()>>>();
    compositor.push(Box::new(overlaid(picker)));
}

fn start(editor: &mut Editor, plan: &Arc<Plan>, destination: Destination) {
    if let Some(what) = editor.git.running(&plan.root) {
        return editor.set_error(format!(
            "A {what} is running in this repository; wait for it or :git-cancel"
        ));
    }
    let label = format!("{}/{}", destination.remote, destination.branch);
    let short = &plan.oid[..plan.oid.len().min(10)];
    editor.set_status(format!("Pushing {short} to {label}…"));
    let plan = Arc::clone(plan);
    let root = plan.root.clone();
    let task = tokio::spawn(async move {
        let result = run(&plan, &destination).await;
        job::dispatch(move |editor, _| finish(editor, &plan, &destination, result)).await;
    });
    editor.git.running.insert(
        root,
        Running {
            what: "push",
            task: task.abort_handle(),
        },
    );
}

/// Push exactly the planned commit if the branch still points at it.
/// Returns whether the remote was already up to date.
async fn run(plan: &Plan, destination: &Destination) -> anyhow::Result<bool> {
    let root = &plan.root;
    let reference = git_text(root, &["symbolic-ref", "-q", "HEAD"])
        .await
        .unwrap_or_default();
    if reference != format!("refs/heads/{}", plan.branch)
        || head(root).await?.as_deref() != Some(plan.oid.as_str())
    {
        bail!("HEAD moved since the push was prepared; run :git-push again");
    }
    let refspec = format!("{}:refs/heads/{}", plan.oid, destination.branch);
    let out = output(
        root,
        &["push", "--porcelain", &destination.remote, &refspec],
        None,
        PUSH,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.success {
        bail!(rejection(
            &stdout,
            &out.message(),
            &format!("{}/{}", destination.remote, destination.branch)
        ));
    }
    if destination.set_upstream {
        let branch = &plan.branch;
        git(
            root,
            &[
                "config",
                &format!("branch.{branch}.remote"),
                &destination.remote,
            ],
        )
        .await?;
        git(
            root,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{}", destination.branch),
            ],
        )
        .await?;
    }
    Ok(stdout.contains("[up to date]"))
}

fn finish(
    editor: &mut Editor,
    plan: &Plan,
    destination: &Destination,
    result: anyhow::Result<bool>,
) {
    if editor
        .git
        .running
        .get(&plan.root)
        .is_some_and(|r| r.what == "push")
    {
        editor.git.running.remove(&plan.root);
    }
    let label = format!("{}/{}", destination.remote, destination.branch);
    let short = &plan.oid[..plan.oid.len().min(10)];
    match result {
        Ok(up_to_date) => {
            let message = format!(
                "{} {short} to {label}{}",
                if up_to_date {
                    "Already pushed:"
                } else {
                    "Pushed"
                },
                if destination.set_upstream {
                    " (upstream set)"
                } else {
                    ""
                }
            );
            editor.set_status(message.clone());
            follow_review(editor, plan, message);
        }
        Err(err) => editor.set_error(safe_text(&format!("{err:#}"))),
    }
}

/// With the branch's PR review loaded: wait until GitHub shows the pushed
/// commit in the PR, then reload the review (placing discussions on the new
/// code) and point at the reply to the selected discussion.
fn follow_review(editor: &mut Editor, plan: &Plan, pushed: String) {
    let Some(context) = editor
        .review
        .context
        .clone()
        .filter(|c| c.root == plan.root && c.branch == plan.branch)
    else {
        return;
    };
    let Some(review) = editor.review.review.clone() else {
        return;
    };
    let next = crate::review::selected_label(editor, &plan.root, &plan.branch)
        .map(|d| format!(" · :review-fixed replies to {d}"))
        .unwrap_or_default();
    let (root, branch, oid) = (plan.root.clone(), plan.branch.clone(), plan.oid.clone());
    tokio::spawn(async move {
        let pull = review.pull().to_owned();
        let mut contained = false;
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            match crate::review::github::pull_head(&context, &review.repo, review.number).await {
                Ok(head) => {
                    contained = head.oid == oid
                        || git(&root, &["merge-base", "--is-ancestor", &oid, &head.oid])
                            .await
                            .is_ok();
                    if contained {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        job::dispatch(move |editor, _| {
            // Say nothing once the user has moved on to another branch or repository.
            let current = current_repo(editor).ok();
            if current.as_ref().map(|r| (&r.root, r.branch.as_ref()))
                != Some((&root, Some(&branch)))
            {
                return;
            }
            if !contained {
                return editor.set_status(format!(
                    "{pushed} · GitHub does not show it in {pull} yet; :review-refresh later"
                ));
            }
            let message = format!("{pushed} · in {pull}{next}");
            editor.set_status(message.clone());
            // Reload the discussions for the new PR head, keeping the selection.
            if editor.review.enabled && editor.review.context.as_ref() == Some(&context) {
                editor.review.after_load = Some((root, branch, message));
                let selection = editor.review.pull.clone();
                crate::review::refresh_editor(editor, selection);
            }
        })
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.into(), b.into()))
    }

    #[test]
    fn destinations_follow_git_configuration() {
        let remotes = [
            "origin".to_owned(),
            "upstream".to_owned(),
            "fork".to_owned(),
        ];
        let names = |d: Vec<Destination>| {
            d.into_iter()
                .map(|d| format!("{}/{} {} {}", d.remote, d.branch, d.set_upstream, d.note))
                .collect::<Vec<_>>()
        };
        // Git's push destination is used as configured, also in triangular setups.
        assert_eq!(
            names(destinations(
                "topic",
                &remotes,
                pair("origin", "topic"),
                pair("origin", "topic"),
                None,
                None
            )),
            ["origin/topic false upstream"]
        );
        assert_eq!(
            names(destinations(
                "topic",
                &remotes,
                pair("upstream", "main"),
                pair("fork", "topic"),
                None,
                None
            )),
            ["fork/topic false configured push destination"]
        );
        // Git cannot decide: the upstream and the local branch name.
        assert_eq!(
            names(destinations(
                "local",
                &remotes,
                pair("origin", "topic"),
                None,
                None,
                None
            )),
            [
                "origin/topic false upstream",
                "origin/local false same name as the local branch"
            ]
        );
        // No upstream: every remote, PR head repository and conventions first.
        assert_eq!(
            names(destinations(
                "topic",
                &remotes,
                None,
                None,
                Some("fork"),
                Some("upstream")
            )),
            [
                "fork/topic true PR head repository · new branch, sets upstream",
                "upstream/topic true new branch, sets upstream",
                "origin/topic true new branch, sets upstream"
            ]
        );
        assert_eq!(
            names(destinations("topic", &remotes, None, None, None, None))[0],
            "origin/topic true new branch, sets upstream"
        );
    }

    #[test]
    fn rejections_are_explained_without_forcing() {
        let fetch_first =
            "To ../remote\n!\trefs/heads/x:refs/heads/x\t[rejected] (fetch first)\nDone\n";
        let text = rejection(fetch_first, "error: failed to push some refs", "origin/x");
        assert!(
            text.starts_with("Push rejected: origin/x has commits"),
            "{text}"
        );
        assert!(text.contains("never force-pushes"));
        let protected =
            "!\tabc:refs/heads/main\t[remote rejected] (protected branch hook declined)\n";
        assert_eq!(
            rejection(protected, "", "origin/main"),
            "Push rejected by the remote: (protected branch hook declined)"
        );
        assert_eq!(
            rejection("", "error: pre-push hook failed\nhint: x", "origin/x"),
            "Push failed: error: pre-push hook failed · hint: x"
        );
    }
}

#[cfg(all(test, feature = "integration", unix))]
mod editor_tests {
    use super::super::editor_tests::{
        committed, editor, expect, fixture, open, pump, sh, status_text, type_message,
    };
    use super::*;
    use crate::{compositor::Compositor, key, review::canonical};
    use helix_view::{editor::Action, input::Event};
    use std::fs;

    struct Rig {
        editor: Editor,
        jobs: job::Jobs,
        compositor: Compositor,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                editor: editor(),
                jobs: job::Jobs::new(),
                compositor: Compositor::new(helix_view::graphics::Rect::new(0, 0, 120, 40)),
            }
        }

        fn cx(&mut self) -> compositor::Context<'_> {
            compositor::Context {
                editor: &mut self.editor,
                jobs: &mut self.jobs,
                scroll: None,
            }
        }

        async fn pump(&mut self, done: impl Fn(&Editor, &mut Compositor) -> bool) {
            pump(&mut self.editor, &mut self.jobs, &mut self.compositor, done).await
        }

        /// Pickers match their items while rendering, before keys act on them.
        fn key(&mut self, key: helix_view::input::KeyEvent) {
            let area = helix_view::graphics::Rect::new(0, 0, 120, 40);
            let mut compositor = std::mem::replace(&mut self.compositor, Compositor::new(area));
            let mut cx = self.cx();
            compositor.render(area, &mut tui::buffer::Buffer::empty(area), &mut cx);
            compositor.handle_event(&Event::Key(key), &mut cx);
            self.compositor = compositor;
        }

        /// `:git-push`, until its picker or an error shows.
        async fn plan(&mut self) {
            while self.compositor.pop().is_some() {}
            push(&mut self.cx());
            self.pump(|e, c| has_push_picker(c) || !status_text(e).starts_with("Preparing"))
                .await;
        }

        /// Push to the selected row and wait for the result.
        async fn confirm(&mut self) {
            self.key(key!(Enter));
            self.pump(|e, _| !status_text(e).starts_with("Pushing"))
                .await;
        }
    }

    /// A bare repository as `origin`, without an upstream for `topic`.
    fn remote(root: &Path) -> PathBuf {
        let bare = root.parent().unwrap().join("remote.git");
        sh(
            root.parent().unwrap(),
            &["init", "--quiet", "--bare", bare.to_str().unwrap()],
        );
        sh(root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        bare
    }

    fn commit_file(root: &Path, name: &str, text: &str) -> String {
        fs::write(root.join(name), text).unwrap();
        sh(root, &["add", name]);
        sh(root, &["commit", "--quiet", "-m", name]);
        sh(root, &["rev-parse", "HEAD"])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pushes_exactly_the_confirmed_commit_and_never_forces() {
        let (_dir, root) = fixture(true);
        let mut rig = Rig::new();
        rig.editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        // No remote at all.
        rig.plan().await;
        expect(&rig.editor, "No Git remote is configured");

        let bare = remote(&root);
        let remote_head =
            |bare: &Path| sh(bare, &["rev-parse", "--verify", "-q", "refs/heads/topic"]);
        let first = commit_file(&root, "c.txt", "c\n");
        // A new branch: every remote is offered and the push sets the upstream.
        rig.plan().await;
        expect(&rig.editor, "Push topic at ");
        let plan_status = status_text(&rig.editor);
        assert!(plan_status.contains("c.txt"), "{plan_status}");
        assert!(has_push_picker(&mut rig.compositor));
        rig.confirm().await;
        expect(&rig.editor, "to origin/topic (upstream set)");
        assert_eq!(remote_head(&bare), first);
        assert_eq!(
            sh(&root, &["config", "branch.topic.merge"]),
            "refs/heads/topic"
        );
        assert!(rig.editor.git.running.is_empty());

        // Preparing twice shows one picker.
        rig.plan().await;
        push(&mut rig.cx());
        rig.pump(|e, _| status_text(e).starts_with("Push topic at "))
            .await;
        rig.compositor.pop();
        assert!(!has_push_picker(&mut rig.compositor));
        // Configured now: one destination, already up to date.
        rig.plan().await;
        rig.confirm().await;
        expect(&rig.editor, "Already pushed:");

        // HEAD moves after confirming the plan: nothing is pushed.
        let second = commit_file(&root, "d.txt", "d\n");
        rig.plan().await;
        let third = commit_file(&root, "e.txt", "e\n");
        rig.confirm().await;
        expect(&rig.editor, "HEAD moved since the push was prepared");
        assert_eq!(remote_head(&bare), first);
        assert_ne!(second, third);

        // The remote gained a commit elsewhere: rejected, never forced.
        let other = root.parent().unwrap().join("other");
        sh(
            root.parent().unwrap(),
            &[
                "clone",
                "--quiet",
                bare.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        sh(&other, &["checkout", "--quiet", "topic"]);
        let theirs = commit_file(&other, "theirs.txt", "x\n");
        sh(&other, &["push", "--quiet", "origin", "topic"]);
        sh(&root, &["fetch", "--quiet", "origin"]);
        rig.plan().await;
        rig.confirm().await;
        expect(
            &rig.editor,
            "Push rejected: origin/topic has commits that are not in your branch",
        );
        expect(&rig.editor, "never force-pushes");
        assert_eq!(remote_head(&bare), theirs);
        assert_eq!(sh(&root, &["rev-parse", "HEAD"]), third);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_push_blocks_others_and_can_be_cancelled() {
        let (_dir, root) = fixture(true);
        let bare = remote(&root);
        let mut rig = Rig::new();
        rig.editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        let hook = root.join(".git/hooks/pre-push");
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "printf '#!/bin/sh\\nsleep 30\\n' > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&hook)
            .status()
            .unwrap();
        assert!(status.success());
        rig.plan().await;
        rig.key(key!(Enter));
        expect(&rig.editor, "Pushing ");
        rig.plan().await;
        expect(&rig.editor, "A push is running in this repository");
        super::super::cancel(&mut rig.editor);
        expect(&rig.editor, "Cancelled the push");
        assert!(rig.editor.git.running.is_empty());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(sh(&bare, &["for-each-ref"]).is_empty());
    }

    fn has_push_picker(compositor: &mut Compositor) -> bool {
        compositor.find::<Overlay<Picker<Target, ()>>>().is_some()
    }

    /// The whole loop: select a discussion, commit a fix, push, and reply with
    /// the fix commit to that discussion, against a fake GitHub CLI.
    #[tokio::test(flavor = "multi_thread")]
    async fn from_discussion_to_pushed_fix_and_reply() {
        let _gh = crate::review::github::TEST_GH_LOCK.lock().await;
        let (dir, root) = fixture(true);
        let base = sh(&root, &["rev-parse", "HEAD"]);
        let bare = remote(&root);
        sh(&root, &["push", "--quiet", "-u", "origin", "topic"]);
        // The PR is discovered through a GitHub remote that is never pushed to.
        sh(
            &root,
            &["remote", "add", "github", "https://github.com/o/r.git"],
        );
        let state = canonical(dir.path()).unwrap();
        fs::write(state.join("oid"), &base).unwrap();
        let thread = format!(
            r#"{{"id":"T1","path":"a.txt","line":1,"diffSide":"RIGHT","isOutdated":false,"isResolved":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"comments":{{"nodes":[{{"author":{{"login":"alice"}},"body":"please fix","diffHunk":"@@","url":"u","originalCommit":{{"oid":"{base}"}}}}],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}"#
        );
        let script = format!(
            r#"#!/bin/sh
input=$(cat)
printf '%s\n' "$input" >> '{log}'
oid=$(cat '{oid}')
case "$input" in
*pullRequests*) printf '%s' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"number":1,"url":"https://github.com/o/r/pull/1","headRefName":"topic","headRepository":{{"nameWithOwner":"o/r"}}}}],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}';;
*"parent {{"*) printf '%s' '{{"data":{{"repository":{{"parent":null}}}}}}';;
*addPullRequestReviewThreadReply*) printf '%s' '{{"data":{{"addPullRequestReviewThreadReply":{{"comment":{{"url":"https://github.com/o/r/pull/1#discussion_r2"}}}}}}}}';;
*reviewThreads*) printf '{{"data":{{"repository":{{"pullRequest":{{"headRefOid":"%s","updatedAt":"now","reviewThreads":{{"nodes":[%s],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}}}' "$oid" '{thread}';;
*PullRequestReviewThread*) printf '%s' '{{"data":{{"node":{thread_node}}}}}' | sed "s/HEAD_OID/$oid/";;
*byteSize*) printf '%s' '{{"data":{{"repository":{{"object":{{"text":"one\ntwo\nthree\n","isBinary":false,"byteSize":14}}}}}}}}';;
*) printf '{{"data":{{"repository":{{"pullRequest":{{"headRefOid":"%s","updatedAt":"now","headRefName":"topic","headRepository":{{"nameWithOwner":"o/r"}}}}}}}}}}' "$oid";;
esac
"#,
            log = state.join("requests").display(),
            oid = state.join("oid").display(),
            thread_node = thread.replacen('{', r#"{"pullRequest":{"headRefOid":"HEAD_OID"},"#, 1),
        );
        let gh = state.join("fake-gh");
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\" && chmod 700 \"$2\"",
                "sh",
            ])
            .arg(&script)
            .arg(&gh)
            .status()
            .unwrap();
        assert!(status.success());
        *crate::review::github::TEST_GH.lock().unwrap() = Some(gh.to_str().unwrap().to_owned());

        let mut rig = Rig::new();
        rig.editor
            .open(&root.join("a.txt"), Action::VerticalSplit)
            .unwrap();
        crate::review::refresh_editor(&mut rig.editor, None);
        rig.pump(|e, _| e.review.review.is_some()).await;
        crate::review::navigate(&mut rig.cx(), false);
        assert_eq!(rig.editor.review.selected, Some(0));

        // Fix, stage and commit; the draft names the discussion.
        fs::write(root.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
        sh(&root, &["add", "a.txt"]);
        let (editor, jobs, compositor) = (&mut rig.editor, &mut rig.jobs, &mut rig.compositor);
        open(editor, jobs, compositor).await;
        assert!(doc!(editor)
            .text()
            .to_string()
            .contains("# Review discussion @alice on a.txt:1: after :git-push"));
        type_message(editor, "fix: a");
        super::super::send(editor, false);
        pump(editor, jobs, compositor, |e, _| committed(e)).await;
        let fix = sh(&root, &["rev-parse", "HEAD"]);
        let committed_status = status_text(editor);
        assert!(
            committed_status.contains("local only, not pushed"),
            "{committed_status}"
        );
        // The new HEAD reloads the review; the selection and the message stay.
        crate::review::synchronize(editor);
        assert!(editor.review.review.is_none());
        pump(editor, jobs, compositor, |e, _| e.review.review.is_some()).await;
        assert_eq!(editor.review.selected, Some(0));
        assert_eq!(status_text(editor), committed_status);

        // Push; GitHub reports the new PR head; the review reloads.
        fs::write(state.join("oid"), &fix).unwrap();
        rig.plan().await;
        rig.confirm().await;
        expect(&rig.editor, "Pushed ");
        rig.pump(|e, _| status_text(e).contains(" · in o/r#1"))
            .await;
        rig.pump(|e, _| e.review.review.is_some()).await;
        expect(&rig.editor, ":review-fixed replies to @alice on a.txt:1");
        assert_eq!(rig.editor.review.selected, Some(0));
        assert_eq!(sh(&bare, &["rev-parse", "refs/heads/topic"]), fix);

        // Reply with the fix commit to that discussion, from any buffer.
        crate::review::reply::fixed(&mut rig.editor, None);
        rig.pump(|e, _| !e.review.compose.is_empty()).await;
        let text = doc!(rig.editor).text().to_string();
        assert!(text.starts_with(&format!("Fixed in {fix}.")), "{text}");
        crate::review::reply::send(&mut rig.editor, false, false);
        rig.pump(|e, _| status_text(e).starts_with("Replied to"))
            .await;
        let requests = fs::read_to_string(state.join("requests")).unwrap();
        let reply = requests
            .lines()
            .find(|l| l.contains("addPullRequestReviewThreadReply"))
            .unwrap();
        let reply: serde_json::Value = serde_json::from_str(reply).unwrap();
        assert_eq!(
            reply["variables"],
            serde_json::json!({"id": "T1", "body": format!("Fixed in {fix}.")})
        );
        *crate::review::github::TEST_GH.lock().unwrap() = None;
    }
}
