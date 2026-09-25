# GitHub review comments

This fork can show the current branch's GitHub review discussions as virtual
blocks below the reviewed code, reply to them, and resolve them. It uses the
installed `gh` CLI and its existing authentication. Run `gh auth status` outside
Helix if authentication needs checking; replying and resolving need write access
to the repository (a token with the `repo` scope). Starting new discussions and
submitting reviews are not implemented.

Open a file on your PR branch and run `:review-toggle`. Helix discovers the open
PR automatically, including PRs from a fork into its parent. There is no initial
PR picker. Run the same command again to hide comments globally. Display starts
disabled in each editor session.

| Command | Action |
| --- | --- |
| `:review-toggle` | Enable/disable review display in all views. |
| `:review-refresh` | Reload the current branch's PR and comments; also enables display. |
| `:review-next`, `:review-prev` | Visit discussions across files, with wraparound. Goes to the code, also when it changed; see [Locations](#locations-and-refresh). |
| `:review-expand` | Expand/collapse the discussion at the cursor or selected by navigation. |
| `:review-open` | Open the complete discussion and original diff in a discussion buffer. |
| `:review-list` | Pick a discussion (first line, current location and placement, author/status/comment count) with a code preview; typing filters by location; `Enter` jumps to the code. Includes outdated and file-level discussions. |
| `:review-select owner/repo#123` | Explicitly select a PR when automatic association is ambiguous. |
| `:review-reply [text]` | Reply to the discussion at the cursor or selected by navigation. With text, post it at once; without, open a reply draft. |
| `:review-send` | Post the reply draft in the current buffer. `:write` (`:w`) in a draft does the same. |
| `:review-fixed [rev]` | Open a reply draft prefilled with `Fixed in <commit>.` (default: the suggested commit, see below). |
| `:review-insert-commit` | Pick a recent branch commit and insert its full id at the cursors. Works in any buffer. |
| `:review-yank-commit` | Put the suggested commit id into register `h` and the system clipboard. |
| `:review-resolve`, `:review-unresolve` | Resolve or reopen the discussion. |

After loading, the status message summarizes the PR, e.g.
`owner/repo#12: 3 discussion(s), 2 open · 2 inline (1 outdated), 1 old-side/file-level`.
Discussions on new code are drawn inline, including those GitHub reports as
outdated. Discussions on removed code (the diff's old side) and file-level
discussions are never drawn inline; reach them with `:review-next` or
`:review-list`, which open the discussion view.

Add the `review` element to a statusline section to keep that state visible
while display is enabled (`reviews: loading`, `repo#12 2/3 open`,
`reviews: no PR`, or `reviews: error` with the reason in the last message):

```toml
[editor.statusline]
right = ["review", "separator", "selections", "position", "file-type"]
```

Several discussions can share a line; next/previous selects each individually.
Collapsed blocks show the author, resolved/open status, comment count and the
first line of the discussion. Expanded blocks include replies. Inline text wraps
to the view width and is limited to 40 rows per discussion; `:review-open` retains
the complete text. Comments are rendered as terminal-safe plain text, including
Markdown source. Links and code fences are not executed.

For example:

```text
  12  let result = parse(input)?;
      │ [+] @reviewer · open · 2 comment(s)
      │ Could this return an error for an empty input?
  13  return result;
```

A block whose code changed since the review says so:

```text
  14  let result = parse(input).unwrap_or_default();
      │ [+] @reviewer · open · outdated · 1 comment(s) · code changed
      │ Could this return an error for an empty input?
```

There are no new default shortcuts. An optional keymap in `config.toml` is:

```toml
[keys.normal.space.R]
t = ":review-toggle"
r = ":review-refresh"
n = ":review-next"
p = ":review-prev"
e = ":review-expand"
o = ":review-open"
l = ":review-list"
a = ":review-reply"
f = ":review-fixed"
c = ":review-insert-commit"
y = ":review-yank-commit"
s = ":review-resolve"
u = ":review-unresolve"
```

With this configuration, press `Space R t` to toggle, `Space R n` to navigate,
`Space R e` to expand and `Space R a` to answer. Closing the discussion buffer with `:buffer-close`
returns to another buffer; `:buffer-previous` also returns to the previous file.

## Replying with the commit that fixed it

The usual loop is: navigate to a discussion, change the code, commit, push, and
answer with the commit. For example:

```text
:review-next                 select the discussion
(edit, commit, git push)
:review-reply                open a reply draft for it
iFixed in <C-r>h, thanks!    type; Ctrl-r h inserts the suggested commit id
:w                           post the reply; the draft closes
:review-resolve              optionally resolve the discussion
```

`:review-reply` opens a Markdown scratch buffer named
`[review-reply] @reviewer on path:line`. Write the reply at the top. Everything
from the separator line down (`<!-- review: everything from this line down is
context and is not sent -->`) is context and is never sent: which discussion the
draft answers, the suggested commit and the quoted conversation. `:w` or
`:review-send` posts the text above the separator; `:review-send --resolve`
also resolves the discussion. A draft is bound to its discussion when it opens,
so moving the cursor, switching branches or refreshing does not re-target it.
Closing a draft with typed text is refused like any unsaved buffer; `:bc!`
discards it. Running `:review-reply` again for the same discussion returns to
the open draft. After posting, only that discussion is reloaded (or the whole
review if the PR head moved), so the inline block, `:review-open` and the
`review` statusline counts show the new reply and state.

The **suggested commit** is the newest commit on the current branch that changed
the discussed lines since the commit the discussion was written against. Helix
follows those lines through intermediate edits (line shifts, replacements, and
insertions directly next to them), so this works for discussions GitHub already
shows as outdated. If no commit touched the lines, the newest commit touching the
file is suggested, otherwise `HEAD`. When a draft opens, the suggestion's full id
is stored in register `h`: paste it with `"hp`, or `Ctrl-r h` in insert mode.
The status line and the draft's context show its short id and subject.

`:review-insert-commit` opens a picker of the branch's recent commits (up to 50,
excluding the PR base branch): subject, short id, date, whether the commit is
already part of the PR head on GitHub (`pushed`/`local`), and whether it
touched the discussed `lines` or `file` (those are listed first). `Enter`
inserts the full commit id at every cursor. `:review-yank-commit` copies the
suggestion for use elsewhere. `:review-fixed [rev]` opens a draft already
containing `Fixed in <commit>.`; pass any revision (`HEAD~1`, a short id, a
branch) to choose another commit.

GitHub links commit ids in PR comments (full and abbreviated ids render as a
short linked id). A commit that is not on GitHub yet cannot be linked, so before
posting, Helix checks every word in the reply that is a local commit id (7-40 hex
digits) against the PR head fetched fresh from GitHub. If one is not contained
in it, posting stops with e.g. `1b5bbb8d77 not on me/helix:topic (PR head
2dab7b8e61) yet; push first, or send anyway with :w! / --force`. `:w!`,
`:review-send --force` or `:review-reply --force …` sends anyway. Hex words that
are not local commits are ignored.

`:review-reply text` posts a one-line reply immediately, for example
`:review-reply --resolve Fixed in %sh{git rev-parse HEAD}`. The text after the
first word is taken literally, apart from `%` expansions; flags (`--resolve`,
`--force`) go before the text. Replies are posted with GitHub's
`addPullRequestReviewThreadReply`; resolution uses `resolveReviewThread` and
`unresolveReviewThread`. Helix checks GitHub's `viewerCanReply` and
`viewerCanResolve` first, and access failures are reported with a hint to check
write access and `gh auth status`. The reply text is sent as a JSON variable on
the `gh` process's standard input; it is never part of the query, a shell
command or a command option.

## Locations and refresh

Review line numbers belong to a revision: the PR head for current discussions,
and the commit a discussion was written against for discussions GitHub reports
as `outdated`. Helix fetches the PR head's file contents from GitHub (and
verifies the PR has not changed while loading) and reads an outdated
discussion's original file from the local repository. It then places each
discussion on new code in the current buffer, saved or unsaved, in one of three
ways:

| Placement | Label | Meaning |
| --- | --- | --- |
| Exact | none | The reviewed lines and three lines of context around them are unchanged and unambiguous. Line shifts from edits elsewhere are followed. |
| Code changed | `code changed` | The reviewed lines or their context changed. Helix carries the lines through a line diff of the reviewed and the current text to the closest remaining code: edited lines stay on their replacement, deleted lines move to the line above them. |
| Approximate | `approximate location` | No reviewed text is available (the original commit is not in the local repository, for example after a rebase, or the file exceeds the size limit), or the lines lie outside it. The discussion is shown at its original line number, limited to the file's last line. |

The label appears on the inline block, in `:review-list` and in the status
message when navigating. It is independent of GitHub's `outdated` state, which
the block, the list and the status message show separately. Undoing a change
restores the exact placement. The placement only decides where a discussion is
shown: replies, resolution and `:review-open` always use the discussion itself,
and `:review-open` still shows the original location, commit, diff side and diff
hunk. Mapping runs in the background and is debounced while typing; results must
still match the document version before being displayed.

Old-side/deleted-code and file-level discussions remain accessible through
navigation and `:review-list`; they open with their original location, commit,
diff side, diff hunk and conversation. They are not placed on current code.
Navigation opens a matching file only if its path is safely inside the repository;
symlinks inside the repository are deliberately not followed. A checkout opened
through a symlinked directory is matched by its resolved location, and navigation
reuses that buffer.

The focused file selects the repository. All matching open buffers show that
repository's discussions. Local branch, HEAD and repository changes clear the
previous inline context and start discovery again. Helix checks the local context
before rendering, before accepting results, and once per second while enabled.
Remote comments are cached until `:review-refresh`, a context change, or a reply
or resolution from Helix; there is no periodic GitHub polling. Delayed results for an earlier context are discarded.
An explicit PR selection lasts until the context changes or display is toggled.

Discovery uses the configured push destination where available, searches the
configured GitHub remotes and the head repository's parent, and verifies the
PR's head repository and branch. If Git cannot resolve the push branch (for
example `push.default=simple` with a local branch named differently from its
upstream), the upstream branch name on the same remote is tried after the local
name. Multiple matches produce an explicit ambiguity
message; use `:review-select` to choose. That command is an intentional explicit
override and can inspect another PR in the current working tree. No PR, detached
HEAD, a buffer outside any Git repository, unavailable credentials, and API
failures produce a status/error message (also when enabling with `:review-toggle`);
retry with `:review-refresh` after correcting the condition.

The initial transport supports `github.com`, ordinary Git repositories and Git
worktrees. Source mapping is limited to 2 MiB per file; larger files use the
approximate placement. Fetches have per-process
30-second timeouts, bounded pagination, a 16 MiB aggregate content budget and a
2,000-thread limit. Exceeding a limit is reported instead of silently truncating
the review. GitHub Enterprise hosts, renamed-path inference, rich Markdown,
mouse interactions, new discussions and review submission are future work.
Suggested commits do not follow file renames or multi-parent history beyond the
first parent.

## Development checks

The deterministic fixtures do not use GitHub credentials or make network calls:

```sh
cargo fmt --all -- --check
cargo test -p helix-term --features integration review --lib
cargo test -p helix-view review --lib
```

Tests exercise actual virtual-row reservation/drawing, scrolling and wrapping,
multiple same-line threads, unsaved edits, symlink rejection, context invalidation,
old-side/outdated anchors, terminal controls, fork identity and a fake CLI that
paginates threads/replies and changes revisions mid-fetch. Placement tests cover
changed, deleted and shifted lines, changed context, ambiguous repeated code,
short, empty and unterminated files, restored exact matches, outdated
discussions read from a temporary repository's history, navigation and list
locations for every placement, and drawing at the end of empty files. Write-side tests record
the fake CLI's requests (mutation payloads as JSON variables, thread ids,
resolve/unresolve, access errors), select suggested commits in temporary Git
repositories, and drive a reply draft in the editor from suggestion to an
unpushed-commit refusal, posting, in-place refresh and resolution.

API references: [GitHub CLI authenticated API](https://cli.github.com/manual/gh_api)
and [GitHub pull request GraphQL objects](https://docs.github.com/en/graphql/reference/pulls).
