# Project notes

This fork can capture a task or note about the code you are reading into a
Markdown notes file for the project, list the open tasks, and take you from a
task back to its code. The notes are plain Markdown in a directory of your
choice, one file per project, so they can live in their own Git repository and
be searched with any tool.

| Key | Command | Action |
| --- | --- | --- |
| `Space n c` | `note_capture` | Open a capture draft for the selected lines in a split below. `:w` adds it under the configured heading and closes the draft; `:bc!` discards it. |
| `Space n n` | `note_open` | Open the project's notes file, creating it if needed. |
| `Space n t` | `note_tasks` | Pick an open task of the project. `Enter` goes to its code (or to the task when it has no link), `Alt-n` to the task in the notes file, `Alt-x` marks it done. |
| `Space n T` | `note_tasks_all` | The same for every notes file in the notes directory. |
| `Space n g` | `note_goto_code` | In a notes file, go to the code that the entry at the cursor links to. |
| `Space n x` | `note_toggle_task` | In a notes file, toggle the task at the cursor between open and done. |

## Configuration

```toml
[editor.notes]
directory = "~/notes/projects"   # one <project>.md per project
heading = "Inbox"                # captures go last under this heading
```

The project is the workspace of the focused file: the nearest directory above
it with `.git`, `.jj`, `.svn` or `.helix`. Its notes file is `<name>.md`, named
after that directory, or `<parent>-<name>.md` when another project with the
same name already owns `<name>.md`.

## The format

```markdown
---
project: /home/me/src/app
---
# app

## Inbox

- [ ] Retry loop ignores the context deadline
  code: internal/client/retry.go:88-91 `do` @3f2a1c9
  > for attempt := 0; attempt < c.maxRetries; attempt++ {
```

The front matter names the project root that links are relative to. A task is
a `- [ ]` item (`- [x]` when done, `- [-]` when cancelled); remove the `[ ] `
in the draft to capture a plain note. The `code:` line holds the path and
1-based lines, the enclosing definition from the language's tags query when
there is one, and the commit at capture time. The quoted line is the first
non-blank captured line.

Tasks exist only in the notes files: the task pickers read them each time, and
`Alt-x` and `note_toggle_task` change the file itself. Edit, move or delete
entries freely; anything that keeps the shape above keeps working.

## Following links when the code changes

Going to the code looks for the quoted line: at the recorded line, else the
nearest line with the same text, so inserted or deleted lines above it do not
matter. When the line itself changed, it goes to the recorded definition, and
otherwise to the recorded line with a warning. A renamed or moved file is
reported, not followed.

Captures and toggles write the notes file on disk. When the notes file is open
with unsaved changes, they refuse instead; save or discard the changes first.
