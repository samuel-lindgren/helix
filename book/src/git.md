# Git status, commits and push

This fork can stage files, review what a commit contains, create the commit and
push it without leaving the editor. It works on the repository of the focused buffer
(or of the working directory) and runs the installed `git` in the background.
Nothing is staged, committed or pushed except by the commands and picker keys
below; opening a view, moving around and saving files never do. For the loop
from a review comment to a pushed fix and a reply, see
[GitHub review comments](./github-reviews.md#from-a-review-comment-to-a-pushed-fix-and-a-reply).

| Command | Action |
| --- | --- |
| `:git-status` | Open the changes picker: staged, unstaged, untracked and conflicted files and buffers with unsaved edits, with a diff preview. |
| `:git-commit` | Open the repository's commit draft (or return to it). `:w` in the draft commits. |
| `:git-push` | Show where the checked-out branch goes and what it sends; `Enter` pushes. Never forced. |
| `:git-cancel` | Cancel a running commit or push, for example one waiting on a slow hook or remote. |

There are no default key bindings. For example, next to the
[review commands](./github-reviews.md) in `config.toml`:

```toml
[keys.normal.space.i]
g = ":git-status"
m = ":git-commit"
P = ":git-push"
x = ":git-cancel"
```

## The changes picker

`:git-status` lists one row per file and side of the index. The status line
shows the branch, its upstream with commits ahead (`↑`) and behind (`↓`), and
the counts:

```text
topic → origin/topic ↑1 · 1 staged, 2 unstaged, 1 untracked · Alt-s stage · Alt-a stage whole file · Alt-u unstage · Alt-c commit · Alt-p push
```

| State | Meaning | Preview |
| --- | --- | --- |
| `unsaved buffer` | A buffer with edits that are not saved. Git cannot see them; save before staging. | The buffer |
| `conflict` | Unresolved merge conflict. | The file |
| `staged …` | Changes in the index: `modified`, `new file`, `deleted`, `renamed`, `type changed`. | The staged diff |
| `unstaged …` | Changes in the working tree that are not staged. | The unstaged diff |
| `untracked` | A file (or directory) Git does not track yet. | The file |

A **partly staged** file (some of its changes staged, others not) has two rows,
`staged modified (partly staged)` and `unstaged modified (partly staged)`, each
previewing its own part.

| Key | Action |
| --- | --- |
| `Enter` (`Ctrl-s`, `Ctrl-v`) | Open the file (in a split). For an unsaved buffer, switch to it. |
| `Alt-s` | Stage the file (`git add`). Refused for a partly staged file, so a partial staging is never extended by accident. |
| `Alt-a` | Stage the whole file, including the unstaged part of a partly staged file. |
| `Alt-u` | Unstage the file (`git restore --staged`, or `git rm --cached` before the first commit). Its changes stay in the working tree. |
| `Alt-c` | Open the commit draft. |
| `Alt-p` | Prepare a push (`:git-push`). |

Typing filters by path. After staging or unstaging, the picker reopens with the
new state on the same file, and an open commit draft is updated. Existing
staging is only changed by these keys. Conflicted files and unsaved buffers
cannot be staged from the picker. Staging individual hunks is not supported.

## Commit drafts

`:git-commit` (or `Alt-c`) opens a scratch buffer named `[git-commit] <branch>`,
highlighted as a Git commit message. Write the message at the top. Everything
from Git's scissors line down is context and is never committed:

```text
fix: handle empty input

Return an empty result instead of an error.
# ------------------------ >8 ------------------------
# Do not modify or remove the line above; everything below it is ignored.
# :w commits the message above it · :w! also commits when staged files have
# unsaved edits · :bc! discards this draft.
#
# On branch topic → origin/topic
# HEAD 1b5bbb8 feat: parse input
#
# Changes to be committed:
#	modified:   src/parse.rs
#
# Changes not staged for commit:
#	README.md
#
diff --git a/src/parse.rs b/src/parse.rs
…
```

The file list and the diff are computed from the index tree the commit will
record, so they show exactly what will be committed. `:w` (or `:write`) in the
draft creates the commit with `git commit --cleanup=whitespace -F -`: Git's hooks
and signing configuration apply, and the message is used as written (lines
starting with `#` above the scissors line are kept). The draft is a buffer
without a file, so `:write-all` and `:wq` never commit.

Before committing, Helix checks that:

- the message is not empty and the scissors line is still there;
- no buffer of a *staged* file has unsaved edits, because the commit would contain
  only the saved and staged version. `:w!` commits anyway. Unsaved buffers of
  other files are listed in the draft for information;
- the branch is still checked out and HEAD and the index still match the draft.
  If anything changed (for example `git add` or a commit from a terminal), no
  commit is created: the draft is updated to the current state, keeping the
  message, and `:w` commits after you have reviewed it;
- something is staged.

After the commit, Helix verifies that Git recorded the reviewed index on top of
the reviewed HEAD. If a hook changed the committed content, the status line says
so. On success the draft closes and the status line shows the new commit, e.g.
`Committed 4e1f0c2a9b fix: handle empty input on topic · local only, not pushed: :git-push`.
Change markers in the gutter of committed files are refreshed.

If Git refuses the commit, for example because a hook or signing fails, the
draft stays open with the message unchanged and the output of Git and its hooks
below the scissors line, under `The last commit attempt failed`. A second `:w`
while a commit is running does not start another one. Amending, rebasing,
history editing and conflict resolution are not part of this workflow.

When a [review discussion](./github-reviews.md) of the branch is selected, the
draft names it (`# Review discussion @reviewer on src/parse.rs:12: after
:git-push, :review-fixed replies with this commit`) and the discussion stays
selected while the review reloads for the new commit.

## Pushing

`:git-push` (or `Alt-p` in the changes picker) prepares a push of the
checked-out branch and shows a picker of destinations. The status line names
the branch and its commit; each row shows the remote branch, how many commits
it would receive, and why it is offered. The preview lists the outgoing
commits. `Enter` pushes to the selected row, `Esc` cancels.

| Situation | Destinations offered |
| --- | --- |
| Git knows where the branch is pushed (`@{push}`: the upstream, or `branch.<name>.pushRemote`, `remote.pushDefault` and `push.default`) | That destination only, noted `upstream` or `configured push destination`. |
| An upstream exists, but Git cannot decide (e.g. `push.default=simple` with an upstream named differently, or no remote-tracking branch yet) | The upstream branch, and a branch named like the local one on the same remote. |
| No upstream | Every remote, with the PR's head repository (when its review is loaded), `remote.pushDefault` and `origin` first. The push sets the chosen branch as upstream. |

A row whose remote-tracking branch has commits that are not in the local
branch says `remote has N more: will be rejected`.

The push sends exactly the commit shown (`git push --porcelain <remote>
<commit>:refs/heads/<branch>`). If the branch moved in the meantime, nothing is
pushed: `HEAD moved since the push was prepared; run :git-push again`. There is
no force push. A rejected push is explained and left to you, without fetching,
merging or rebasing: for a remote with new commits, `Push rejected: origin/topic
has commits that are not in your branch. Integrate them outside Helix (for
example git pull --rebase), then push again; Helix never force-pushes`; for
server-side rules, `Push rejected by the remote: (protected branch hook
declined)`; for other failures (a `pre-push` hook, authentication), `Push
failed:` with Git's first lines.

On success the status line says `Pushed <commit> to origin/topic` (`Already
pushed:` when the remote had it). With the branch's PR review loaded, Helix then
asks GitHub until the PR head contains the commit (up to about ten seconds),
reloads the review so that discussions are placed on the new code, and shows
`… · in owner/repo#12 · :review-fixed replies to @reviewer on src/parse.rs:12`,
or `GitHub does not show it in owner/repo#12 yet` if it does not arrive in time.

At most one commit or push runs per repository; a second one is refused until
it finishes or `:git-cancel` stops it.

## Background processes

Git runs with fixed argument lists and literal pathspecs (never through a
shell), in the repository root, without `GIT_DIR`-style overrides from the
editor's environment. It runs with `GIT_TERMINAL_PROMPT=0` and, on Unix, in its
own session without a terminal, so it cannot draw a prompt over the editor. Operations
that would need a password, passphrase or PIN entry on the terminal fail with
Git's message instead; use a credential helper, an SSH agent, or a graphical or
cached pinentry for signing. Status, diffs and staging time out after 30 seconds,
pushes after 5 minutes and commits after 10 minutes. A timeout or `:git-cancel` terminates Git and the
processes it started (hooks, SSH) with `SIGTERM`, which lets Git remove its lock
files, and with `SIGKILL` two seconds later if they are still running.

## Development checks

```sh
cargo test -p helix-view git --lib
cargo test -p helix-term --features integration --lib -- git:: process::
cargo test -p helix-term --features integration --test integration -- test::git
```

The tests use temporary repositories without the developer's Git configuration
or hooks. They cover porcelain status parsing, per-file diff previews, staging
and unstaging without losing existing index content (including partly staged,
new and not-yet-committed files), picker keys, the commit draft (empty message,
unsaved buffers, changed index or HEAD, another branch, failing and
index-changing hooks, duplicate `:w`, cancellation and lock cleanup, the first
commit), pushing to a local bare remote (destination choice, upstream setup,
already pushed, moved HEAD, rejection without force, a running push blocking
another and cancellation), the loop from a selected review discussion through a
commit and push to the reply with the fix commit against a fake GitHub CLI, and
the keyboard flow from an edit to a pushed commit in a running editor.
