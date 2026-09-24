# GitHub review comments

This fork can show the current branch's GitHub review discussions as virtual
blocks below the reviewed code. It uses the installed `gh` CLI and its existing
authentication. Run `gh auth status` outside Helix if authentication needs checking.
The feature only reads GitHub; replies, resolution changes and review submission
are not implemented.

Open a file on your PR branch and run `:review-toggle`. Helix discovers the open
PR automatically, including PRs from a fork into its parent. There is no initial
PR picker. Run the same command again to hide comments globally. Display starts
disabled in each editor session.

| Command | Action |
| --- | --- |
| `:review-toggle` | Enable/disable review display in all views. |
| `:review-refresh` | Reload the current branch's PR and comments; also enables display. |
| `:review-next`, `:review-prev` | Visit discussions across files, with wraparound. |
| `:review-expand` | Expand/collapse the discussion at the cursor or selected by navigation. |
| `:review-open` | Open the complete discussion and original diff in a discussion buffer. |
| `:review-list` | List the current PR's discussions, including unavailable locations. |
| `:review-select owner/repo#123` | Explicitly select a PR when automatic association is ambiguous. |

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
```

With this configuration, press `Space R t` to toggle, `Space R n` to navigate,
and `Space R e` to expand. Closing the discussion buffer with `:buffer-close`
returns to another buffer; `:buffer-previous` also returns to the previous file.

## Locations and refresh

Review line numbers belong to a PR revision. Helix fetches that immutable head's
file contents, verifies the PR has not changed while loading, and maps only
unchanged, unambiguous context into the current buffer. This includes line shifts
from saved or unsaved local edits. Editing the reviewed text, edits within its
three-line context window, ambiguous repeated code, unsupported paths, and missing
or oversized source blobs may make the location unavailable. Undoing the change
can restore the mapping. Helix never guesses a replacement line. Mapping runs in
the background and is debounced while typing; results must still match the
document version before being displayed.

Old-side/deleted-code, outdated, and file-level discussions remain accessible
through navigation and `:review-list`; they open with their original location,
commit, diff side, diff hunk and conversation. They are not placed on current code.
Navigation opens a matching file only if its path is safely inside the repository;
paths through symlinks are deliberately not followed.

The focused file selects the repository. All matching open buffers show that
repository's discussions. Local branch, HEAD and repository changes clear the
previous inline context and start discovery again. Helix checks the local context
before rendering, before accepting results, and once per second while enabled.
Remote comments are cached until `:review-refresh` or a context change; there is
no periodic GitHub polling. Delayed results for an earlier context are discarded.
An explicit PR selection lasts until the context changes or display is toggled.

Discovery uses the configured push destination where available, searches the
configured GitHub remotes and the head repository's parent, and verifies the
PR's head repository and branch. Multiple matches produce an explicit ambiguity
message; use `:review-select` to choose. That command is an intentional explicit
override and can inspect another PR in the current working tree. No PR, detached
HEAD, unavailable credentials, and API failures produce a status/error message;
retry with `:review-refresh` after correcting the condition.

The initial transport supports `github.com`, ordinary Git repositories and Git
worktrees. Source mapping is limited to 2 MiB per file. Fetches have per-process
30-second timeouts, bounded pagination, a 16 MiB aggregate content budget and a
2,000-thread limit. Exceeding a limit is reported instead of silently truncating
the review. GitHub Enterprise hosts, renamed-path inference, rich Markdown,
mouse interactions and writing reviews are future work.

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
paginates threads/replies and changes revisions mid-fetch.

API references: [GitHub CLI authenticated API](https://cli.github.com/manual/gh_api)
and [GitHub pull request GraphQL objects](https://docs.github.com/en/graphql/reference/pulls).
