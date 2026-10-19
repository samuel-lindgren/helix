//! The Markdown of a project notes file: entries under headings, tasks and
//! their `code:` links. Pure text functions, so they work on buffers and files
//! alike.
//!
//! ```markdown
//! ---
//! project: /home/me/src/app
//! ---
//! # app
//!
//! ## Inbox
//!
//! - [ ] Retry loop ignores the context deadline
//!   code: internal/client/retry.go:88-91 `do` @3f2a1c9
//!   > for attempt := 0; attempt < c.maxRetries; attempt++ {
//! ```
//!
//! Lines in `code:` links are 1-based. The quoted line is the first non-blank
//! line of the capture, used to find the code again after lines move.

use std::path::{Path, PathBuf};

/// The line that separates a capture draft's text from its context.
pub const CAPTURE_SEPARATOR: &str = "──── capture ────";

/// Longest anchored line kept in a link. Longer lines are not quoted.
const MAX_SNIPPET: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Done,
    Cancelled,
}

/// A location in a project that an entry points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Path relative to the project root, or absolute.
    pub path: String,
    /// First and last line, 0-based.
    pub start: usize,
    pub end: usize,
    pub symbol: Option<String>,
    pub commit: Option<String>,
    /// The trimmed anchored line.
    pub snippet: Option<String>,
}

impl Link {
    pub fn describe(&self) -> String {
        let mut text = format!("{}:{}", self.path, self.start + 1);
        if let Some(symbol) = &self.symbol {
            text.push_str(&format!(" {symbol}"));
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// 0-based line of the task in the notes file.
    pub line: usize,
    pub status: Status,
    pub text: String,
    pub link: Option<Link>,
}

/// The snippet worth quoting from the anchored lines: the first non-blank one.
pub fn snippet<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let line = lines.into_iter().map(str::trim).find(|l| !l.is_empty())?;
    (line.chars().count() <= MAX_SNIPPET).then(|| line.to_owned())
}

/// The `code:` line and the quoted snippet, indented as an entry's
/// continuation.
pub fn render_anchor(link: &Link) -> String {
    let mut text = format!("  code: {}:{}", link.path, link.start + 1);
    if link.end > link.start {
        text.push_str(&format!("-{}", link.end + 1));
    }
    if let Some(symbol) = &link.symbol {
        text.push_str(&format!(" `{symbol}`"));
    }
    if let Some(commit) = &link.commit {
        text.push_str(&format!(" @{commit}"));
    }
    text.push('\n');
    if let Some(snippet) = &link.snippet {
        text.push_str(&format!("  > {snippet}\n"));
    }
    text
}

/// The draft text of a capture: an empty task above the separator, the
/// context below it.
pub fn capture_draft(anchor: &str, heading: &str, file: &str) -> String {
    format!(
        "- [ ] \n\n{CAPTURE_SEPARATOR}\n{anchor}:w adds the text above under \"{heading}\" in {file}; :bc! discards.\nRemove \"[ ] \" to make it a note instead of a task.\n"
    )
}

/// The cursor offset in a fresh capture draft: after `- [ ] `.
pub const CAPTURE_CURSOR: usize = 6;

/// The entry text that a capture draft holds: everything above the separator.
pub fn capture_body(draft: &str) -> &str {
    draft
        .split_once(&format!("\n{CAPTURE_SEPARATOR}"))
        .map_or(draft, |(body, _)| body)
}

/// A list entry from a capture's text and anchor. `None` when the text is
/// empty or only an empty task marker.
pub fn entry(body: &str, anchor: &str) -> Option<String> {
    let mut lines = body
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty());
    let first = lines.next()?.trim_start();
    if matches!(first, "-" | "- [ ]" | "- [x]" | "- [X]" | "- [-]") {
        return None;
    }
    let mut text = if first.starts_with("- ") {
        first.to_owned()
    } else {
        format!("- {first}")
    };
    text.push('\n');
    for line in lines {
        text.push_str(&format!("  {}\n", line.trim_start()));
    }
    text.push_str(anchor);
    Some(text)
}

/// A new notes file for a project.
pub fn new_file(project: &Path, name: &str, heading: &str) -> String {
    format!(
        "---\nproject: {}\n---\n# {name}\n\n## {heading}\n\n",
        project.display()
    )
}

/// The project root that a notes file's front matter names.
pub fn project_root(text: &str) -> Option<PathBuf> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim_end() != "---")
        .find_map(|line| line.strip_prefix("project:"))
        .map(|root| PathBuf::from(root.trim()))
        .filter(|root| !root.as_os_str().is_empty())
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((level, rest.trim()))
}

/// Where to insert `entry` so that it ends up last under `heading`, and the
/// text to insert there: `(char offset, insertion)`. Adds the heading at the
/// end of the file when it is missing.
pub fn insert_under_heading(text: &str, heading: &str, entry: &str) -> (usize, String) {
    // (byte offset of the line start, line without its ending)
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        lines.push((offset, line.trim_end_matches(['\n', '\r'])));
        offset += line.len();
    }
    let char_offset = |byte: usize| text[..byte].chars().count();

    let found = lines.iter().enumerate().find_map(|(i, (_, line))| {
        heading_level(line)
            .filter(|(_, title)| *title == heading)
            .map(|(level, _)| (i, level))
    });
    let Some((head, level)) = found else {
        let mut insertion = String::new();
        if !text.is_empty() {
            if !text.ends_with('\n') {
                insertion.push('\n');
            }
            if !text.ends_with("\n\n") {
                insertion.push('\n');
            }
        }
        insertion.push_str(&format!("## {heading}\n\n{entry}"));
        return (text.chars().count(), insertion);
    };

    let section_end = lines[head + 1..]
        .iter()
        .position(|(_, line)| heading_level(line).is_some_and(|(l, _)| l <= level))
        .map_or(lines.len(), |i| head + 1 + i);
    let last = (head..section_end)
        .rev()
        .find(|&i| !lines[i].1.trim().is_empty())
        .unwrap_or(head);
    // An empty section keeps one blank line after its heading.
    let at = if last == head && head + 1 < section_end {
        head + 2
    } else {
        last + 1
    };
    let position = lines.get(at).map_or(text.len(), |(start, _)| *start);

    let mut insertion = String::new();
    if position == text.len() && !text.is_empty() && !text.ends_with('\n') {
        insertion.push('\n');
    }
    if at == head + 1 {
        insertion.push('\n');
    }
    insertion.push_str(entry);
    if at == section_end && section_end < lines.len() {
        // Keep a blank line before the next heading.
        insertion.push('\n');
    }
    (char_offset(position), insertion)
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn checkbox(line: &str) -> Option<(Status, &str)> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let mut chars = rest.chars();
    let status = match chars.next()? {
        ' ' => Status::Open,
        'x' | 'X' => Status::Done,
        '-' => Status::Cancelled,
        _ => return None,
    };
    let text = chars.as_str().strip_prefix(']')?;
    if !(text.is_empty() || text.starts_with(' ')) {
        return None;
    }
    Some((status, text.trim()))
}

fn is_item(line: &str) -> bool {
    let line = line.trim_start();
    line == "-" || line.starts_with("- ")
}

/// Parses a `code:` line.
pub fn parse_link(line: &str) -> Option<Link> {
    let rest = line.trim().strip_prefix("code:")?.trim();
    let mut tokens = rest.split_whitespace();
    let (path, range) = tokens.next()?.rsplit_once(':')?;
    let (start, end) = match range.split_once('-') {
        Some((start, end)) => (start.parse::<usize>().ok()?, end.parse::<usize>().ok()?),
        None => {
            let line = range.parse::<usize>().ok()?;
            (line, line)
        }
    };
    if path.is_empty() || start == 0 || end < start {
        return None;
    }
    let mut link = Link {
        path: path.to_owned(),
        start: start - 1,
        end: end - 1,
        symbol: None,
        commit: None,
        snippet: None,
    };
    for token in tokens {
        if let Some(commit) = token.strip_prefix('@') {
            link.commit = Some(commit.to_owned());
        } else if let Some(symbol) = token.strip_prefix('`').and_then(|t| t.strip_suffix('`')) {
            link.symbol = Some(symbol.to_owned());
        }
    }
    Some(link)
}

/// The continuation lines of the list item starting at `item`.
fn continuation<'a, 'b>(lines: &'b [&'a str], item: usize) -> impl Iterator<Item = &'a str> + 'b {
    let base = indent(lines[item]);
    lines[item + 1..]
        .iter()
        .take_while(move |line| !line.trim().is_empty() && indent(line) > base)
        .copied()
}

fn item_link(lines: &[&str], item: usize) -> Option<Link> {
    let mut rest = continuation(lines, item);
    while let Some(line) = rest.next() {
        if let Some(mut link) = parse_link(line) {
            link.snippet = rest
                .next()
                .and_then(|line| line.trim().strip_prefix('>'))
                .map(|snippet| snippet.trim().to_owned())
                .filter(|snippet| !snippet.is_empty());
            return Some(link);
        }
    }
    None
}

/// The list item containing `line`: the line itself or the nearest item above
/// whose continuation reaches it.
fn item_at(lines: &[&str], line: usize) -> Option<usize> {
    let mut want = indent(lines.get(line)?);
    for i in (0..=line).rev() {
        let text = lines[i];
        if text.trim().is_empty() {
            return None;
        }
        if is_item(text) && (i == line || indent(text) < want) {
            return Some(i);
        }
        want = want.min(indent(text));
        if want == 0 && i != line {
            return None;
        }
    }
    None
}

/// The code link of the entry at `line`.
pub fn link_at(text: &str, line: usize) -> Option<Link> {
    let lines: Vec<&str> = text.lines().collect();
    item_at(&lines, line).and_then(|item| item_link(&lines, item))
}

/// The line of the task at `line`, if `line` is in one.
pub fn task_at(text: &str, line: usize) -> Option<usize> {
    let lines: Vec<&str> = text.lines().collect();
    item_at(&lines, line).filter(|&item| checkbox(lines[item]).is_some())
}

/// All tasks in a notes file.
pub fn tasks(text: &str) -> Vec<Task> {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let (status, text) = checkbox(line)?;
            Some(Task {
                line: i,
                status,
                text: text.to_owned(),
                link: item_link(&lines, i),
            })
        })
        .collect()
}

/// The task line with its status toggled between open and done. A cancelled
/// task reopens.
pub fn toggle(line: &str) -> Option<String> {
    let (status, _) = checkbox(line)?;
    let at = line.find("- [")? + 3;
    let mark = if status == Status::Open { 'x' } else { ' ' };
    let mut toggled = line.to_owned();
    toggled.replace_range(at..at + 1, &mark.to_string());
    Some(toggled)
}

/// The line that the anchored snippet is on now: `line` when it still is, else
/// the matching line nearest to it.
pub fn relocate<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    line: usize,
    snippet: &str,
) -> Option<usize> {
    lines
        .into_iter()
        .enumerate()
        .filter(|(_, text)| text.trim() == snippet)
        .map(|(i, _)| i)
        .min_by_key(|&i| i.abs_diff(line))
}

#[cfg(test)]
mod test {
    use super::*;

    fn link() -> Link {
        Link {
            path: "src/lib.rs".into(),
            start: 87,
            end: 90,
            symbol: Some("run".into()),
            commit: Some("3f2a1c9".into()),
            snippet: Some("let x = 1;".into()),
        }
    }

    #[test]
    fn anchor_round_trip() {
        let anchor = render_anchor(&link());
        assert_eq!(
            anchor,
            "  code: src/lib.rs:88-91 `run` @3f2a1c9\n  > let x = 1;\n"
        );
        let entry = entry("- [ ] Fix it", &anchor).unwrap();
        let tasks = tasks(&entry);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, Status::Open);
        assert_eq!(tasks[0].text, "Fix it");
        assert_eq!(tasks[0].link, Some(link()));
    }

    #[test]
    fn single_line_link() {
        let link = parse_link("code: a.go:3").unwrap();
        assert_eq!((link.start, link.end), (2, 2));
        assert_eq!(parse_link("code: a.go:0"), None);
        assert_eq!(parse_link("code: a.go:x"), None);
    }

    #[test]
    fn entries_from_drafts() {
        let draft = capture_draft("  code: a.go:1\n", "Inbox", "app.md");
        assert_eq!(&draft[..CAPTURE_CURSOR], "- [ ] ");
        assert_eq!(entry(capture_body(&draft), "A"), None);
        assert_eq!(
            entry("Plain note\n\n  more\n", "  code: a.go:1\n").unwrap(),
            "- Plain note\n  more\n  code: a.go:1\n"
        );
        let typed = draft.replacen("- [ ] ", "- [ ] Do it", 1);
        assert_eq!(capture_body(&typed), "- [ ] Do it\n");
    }

    #[test]
    fn insert_into_existing_heading() {
        let text = "# app\n\n## Inbox\n\n- [ ] one\n\n## Later\n\n- [ ] two\n";
        let (at, insertion) = insert_under_heading(text, "Inbox", "- [ ] new\n");
        let mut result = text.to_owned();
        result.insert_str(at, &insertion);
        assert_eq!(
            result,
            "# app\n\n## Inbox\n\n- [ ] one\n- [ ] new\n\n## Later\n\n- [ ] two\n"
        );
    }

    #[test]
    fn insert_into_empty_heading_and_tight_sections() {
        let text = "## Inbox\n## Later\n";
        let (at, insertion) = insert_under_heading(text, "Inbox", "- a\n");
        let mut result = text.to_owned();
        result.insert_str(at, &insertion);
        assert_eq!(result, "## Inbox\n\n- a\n\n## Later\n");

        let text = new_file(Path::new("/p"), "p", "Inbox");
        let (at, insertion) = insert_under_heading(&text, "Inbox", "- a\n");
        let mut result = text.clone();
        result.insert_str(at, &insertion);
        assert!(result.ends_with("## Inbox\n\n- a\n"), "{result:?}");
    }

    #[test]
    fn insert_adds_missing_heading() {
        let text = "# app\nno newline";
        let (at, insertion) = insert_under_heading(text, "Inbox", "- a\n");
        assert_eq!(at, text.chars().count());
        assert_eq!(insertion, "\n\n## Inbox\n\n- a\n");
        // Deeper headings with the same title count; subsections stay inside.
        let text = "### Inbox\n- a\n#### Sub\n- b\n# Next\n";
        let (at, insertion) = insert_under_heading(text, "Inbox", "- c\n");
        let mut result = text.to_owned();
        result.insert_str(at, &insertion);
        assert_eq!(result, "### Inbox\n- a\n#### Sub\n- b\n- c\n\n# Next\n");
    }

    #[test]
    fn front_matter() {
        let text = new_file(Path::new("/home/me/app"), "app", "Inbox");
        assert_eq!(project_root(&text), Some(PathBuf::from("/home/me/app")));
        assert_eq!(project_root("# app\nproject: /x\n"), None);
    }

    #[test]
    fn links_and_tasks_at_lines() {
        let text = "## Inbox\n\n- [ ] one\n  details\n  code: a.go:3\n  > x := 1\n- note\n  code: b.go:4\n\n- [x] done\n";
        assert_eq!(link_at(text, 3).unwrap().path, "a.go");
        assert_eq!(link_at(text, 5).unwrap().snippet.as_deref(), Some("x := 1"));
        assert_eq!(link_at(text, 6).unwrap().path, "b.go");
        assert_eq!(link_at(text, 0), None);
        assert_eq!(task_at(text, 4), Some(2));
        assert_eq!(task_at(text, 7), None);
        let tasks = tasks(text);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(tasks[1].link, None);
    }

    #[test]
    fn toggles() {
        assert_eq!(toggle("  - [ ] a").as_deref(), Some("  - [x] a"));
        assert_eq!(toggle("- [X] a").as_deref(), Some("- [ ] a"));
        assert_eq!(toggle("- [-] a").as_deref(), Some("- [ ] a"));
        assert_eq!(toggle("- a"), None);
        assert_eq!(toggle("- [ ]x"), None);
    }

    #[test]
    fn relocates_to_nearest_match() {
        let lines = ["a", "  x", "b", "c", "x", "d"];
        assert_eq!(relocate(lines, 1, "x"), Some(1));
        assert_eq!(relocate(lines, 3, "x"), Some(4));
        assert_eq!(relocate(lines, 0, "y"), None);
        assert_eq!(
            snippet(["", "  ", "  let a = 1;  "]),
            Some("let a = 1;".into())
        );
    }
}
