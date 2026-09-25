//! Review data, reply drafts and document placement. No GitHub or process I/O.
use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};

use imara_diff::{Algorithm, Diff, InternedInput};

use helix_core::{
    doc_formatter::FormattedGrapheme,
    text_annotations::LineAnnotation,
    unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr},
    Position, Rope,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub branch: String,
    pub head: String,
    pub config: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct Thread {
    /// GraphQL node id; replies and resolution changes address this id only.
    pub id: String,
    pub path: String,
    /// One-based inclusive range at the fetched PR head. None for old-side,
    /// outdated, file-level or otherwise uncertain locations.
    pub lines: Option<Range<usize>>,
    pub location: String,
    pub resolved: bool,
    /// GitHub reports the discussion as outdated: its diff hunk changed on the
    /// PR head. Independent of local edits and of the local placement.
    pub outdated: bool,
    pub comments: Vec<Comment>,
    pub diff: String,
    pub url: String,
    /// Commit the discussion was written against (validated hex object id).
    pub commit: Option<String>,
    /// One-based inclusive range in `commit`, for right-side (new code) discussions.
    pub original_lines: Option<Range<usize>>,
    pub can_reply: bool,
    pub can_resolve: bool,
    pub can_unresolve: bool,
}

impl Thread {
    pub fn title(&self) -> String {
        let author = self.comments.first().map_or("[deleted]", |c| &c.author);
        format!(
            "@{author} · {}{} · {} comment(s)",
            if self.resolved { "resolved" } else { "open" },
            if self.outdated { " · outdated" } else { "" },
            self.comments.len()
        )
    }

    /// Right-side discussion with a line range, current or original, that can
    /// be shown on the code. Old-side and file-level discussions cannot.
    pub fn placeable(&self) -> bool {
        self.lines.is_some() || self.original_lines.is_some()
    }

    pub fn conversation(&self) -> String {
        self.comments
            .iter()
            .map(|c| format!("@{}\n{}", c.author, c.body))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Review {
    pub label: String,
    /// Base repository (`owner/repo`) and number of the pull request.
    pub repo: String,
    pub number: u64,
    /// PR head revision all thread line numbers and `sources` belong to.
    pub head: String,
    pub threads: Vec<Arc<Thread>>,
    pub sources: HashMap<String, String>,
    /// File contents at an outdated discussion's original commit, by
    /// `(commit, path)`, read from the local repository when available.
    pub originals: HashMap<(String, String), String>,
    /// The PR's head repository (`owner/repo`) when discovered from the branch.
    pub head_repo: Option<String>,
}

impl Review {
    /// `owner/repo#123`, without the revision suffix of `label`.
    pub fn pull(&self) -> &str {
        self.label.split(" @ ").next().unwrap_or(&self.label)
    }

    /// Counts that explain why fewer discussions may be visible inline.
    pub fn summary(&self) -> String {
        let total = self.threads.len();
        if total == 0 {
            return format!("{}: no review discussions", self.pull());
        }
        let open = self.threads.iter().filter(|t| !t.resolved).count();
        let inline = self.threads.iter().filter(|t| t.placeable()).count();
        let outdated = self
            .threads
            .iter()
            .filter(|t| t.placeable() && t.lines.is_none())
            .count();
        format!(
            "{}: {total} discussion(s), {open} open · {inline} inline ({outdated} outdated), {} old-side/file-level · :review-next, :review-list",
            self.pull(),
            total - inline,
        )
    }
}

#[derive(Clone, Copy)]
pub struct PendingSelection {
    pub index: usize,
    pub document: crate::DocumentId,
    pub view: crate::ViewId,
    pub cursor: usize,
    pub version: i32,
}

/// A reply being written in a scratch buffer. Bound to one discussion node id
/// when opened; posting never re-targets it to whatever is under the cursor.
#[derive(Clone, Debug)]
pub struct Compose {
    pub context: Context,
    pub repo: String,
    pub number: u64,
    pub thread: Arc<Thread>,
    /// A post is in flight; a second `:w` must not send a duplicate.
    pub sending: bool,
}

/// First line of the non-sent context section of a reply buffer.
pub const REPLY_SEPARATOR: &str =
    "<!-- review: everything from this line down is context and is not sent -->";

/// The text above the separator, without surrounding blank lines. `Err` explains
/// why nothing may be sent.
pub fn reply_body(text: &str) -> Result<String, &'static str> {
    let mut body = Vec::new();
    let mut separated = false;
    for line in text.lines() {
        if line.trim() == REPLY_SEPARATOR {
            separated = true;
            break;
        }
        body.push(line.trim_end());
    }
    if !separated {
        return Err("Reply separator line was removed; restore it so quoted context is not sent");
    }
    let body = body.join("\n").trim_matches('\n').to_owned();
    if body.trim().is_empty() {
        return Err("Reply is empty; write it above the separator line");
    }
    Ok(body)
}

#[derive(Default)]
pub struct State {
    pub enabled: bool,
    pub generation: u64,
    pub context: Option<Context>,
    pub loading: bool,
    pub task: Option<tokio::task::AbortHandle>,
    pub pull: Option<String>,
    pub review: Option<Arc<Review>>,
    pub expanded: HashSet<String>,
    pub selected: Option<usize>,
    pub pending_selection: Option<PendingSelection>,
    pub document: Option<crate::DocumentId>,
    pub status: String,
    /// The last load failed or no reviewable context exists; `status` explains.
    pub failed: bool,
    /// Reply drafts by scratch document. Survive refreshes and context changes.
    pub compose: HashMap<crate::DocumentId, Compose>,
    /// The last selected discussion as (repository root, branch, thread id).
    /// Survives the reload after a commit or push on the same branch.
    pub remembered: Option<(PathBuf, String, String)>,
    /// A status message for the next load in (repository root, branch),
    /// shown instead of the summary: the result of the Git action that caused it.
    pub after_load: Option<(PathBuf, String, String)>,
}

impl State {
    pub fn invalidate(&mut self, context: Option<Context>) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.pull = None;
        self.generation = self.generation.wrapping_add(1);
        self.context = context;
        self.loading = false;
        self.failed = false;
        self.review = None;
        self.selected = None;
        self.pending_selection = None;
        self.expanded.clear();
    }

    pub fn accepts(&self, generation: u64, context: &Context) -> bool {
        self.enabled && self.generation == generation && self.context.as_ref() == Some(context)
    }

    /// Compact state for the statusline; `None` while display is disabled.
    pub fn indicator(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        Some(if let Some(review) = &self.review {
            let pull = review.pull();
            let pull = pull.rsplit('/').next().unwrap_or(pull);
            let open = review.threads.iter().filter(|t| !t.resolved).count();
            format!("{pull} {open}/{} open", review.threads.len())
        } else if self.loading {
            "reviews: loading".into()
        } else if self.failed {
            "reviews: error".into()
        } else {
            "reviews: no PR".into()
        })
    }
}

#[derive(Default, Debug)]
pub struct DocumentReview {
    pub stamp: Option<(u64, i32, PathBuf)>,
    pub blocks: Vec<Block>,
    pub task: Option<tokio::task::AbortHandle>,
}
impl Drop for DocumentReview {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// How a discussion's position in the current document was derived. The
/// discussion's own location (path, lines, commit, diff) is never changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// The reviewed lines and their context are unchanged and unambiguous.
    #[default]
    Exact,
    /// The file changed since the reviewed version; the lines were carried
    /// through a line diff to the closest remaining code.
    Changed,
    /// No position could be derived; shown at the original line number.
    Approximate,
}

impl Placement {
    /// Marker for blocks, lists and status messages; empty when exact.
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "",
            Self::Changed => "code changed",
            Self::Approximate => "approximate location",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub thread: Arc<Thread>,
    pub range: Range<usize>,
    /// Anchor at the terminating newline (or EOF), so soft-wrapped code is
    /// completely rendered before the discussion is inserted.
    pub anchor: usize,
    pub expanded: bool,
    pub placement: Placement,
}

/// A line diff hunk in `git diff -U0` terms: one-based starts, and for an
/// empty side the start is the line after which the change happens (0 at the top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}

/// Line hunks turning `before` into `after`, as `git diff -U0` would report them.
pub fn line_hunks(before: &str, after: &str) -> Vec<Hunk> {
    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let side = |range: Range<u32>| {
        let len = (range.end - range.start) as usize;
        let start = range.start as usize;
        (if len == 0 { start } else { start + 1 }, len)
    };
    diff.hunks()
        .map(|hunk| {
            let (old_start, old_len) = side(hunk.before);
            let (new_start, new_len) = side(hunk.after);
            Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
            }
        })
        .collect()
}

/// Carry a one-based inclusive line range through one change's zero-context
/// hunks. Returns whether the change touched the range and the range afterwards.
/// A touching hunk widens the range to its new lines so that follow-up edits of
/// the replacement code are recognized too.
pub fn track(range: Range<usize>, hunks: &[Hunk]) -> (bool, Range<usize>) {
    let (start, end) = (range.start, range.end - 1);
    // A hunk lies entirely before line `line` in old coordinates.
    let before = |h: &Hunk, line: usize| {
        if h.old_len == 0 {
            h.old_start < line
        } else {
            h.old_start + h.old_len - 1 < line
        }
    };
    let map = |line: usize, last: bool| {
        let mut shift: isize = 0;
        for h in hunks {
            if before(h, line) {
                shift += h.new_len as isize - h.old_len as isize;
            } else if h.old_len > 0 && h.old_start <= line {
                // The line itself was replaced or deleted.
                return if last && h.new_len > 0 {
                    h.new_start + h.new_len - 1
                } else if h.new_len > 0 {
                    h.new_start
                } else {
                    h.new_start.max(1)
                };
            } else {
                break;
            }
        }
        (line as isize + shift).max(1) as usize
    };
    let (mut new_start, mut new_end) = (map(start, false), map(end, true));
    let mut touched = false;
    for h in hunks {
        let hit = if h.old_len == 0 {
            // Insertions inside the range or directly adjacent to it.
            h.old_start + 1 >= start && h.old_start <= end
        } else {
            h.old_start <= end && h.old_start + h.old_len > start
        };
        if hit {
            touched = true;
            if h.new_len > 0 {
                new_start = new_start.min(h.new_start);
                new_end = new_end.max(h.new_start + h.new_len - 1);
            }
        }
    }
    (touched, new_start..new_end.max(new_start) + 1)
}

/// Characters of the one-based `lines` in `text`, clamped to its content lines
/// so that short and empty documents still get a valid (possibly empty) range.
pub fn line_range(text: &Rope, lines: Range<usize>) -> Range<usize> {
    let len = text.len_chars();
    if len == 0 {
        return 0..0;
    }
    // The line holding the final character; a trailing newline adds no line.
    let last = text.char_to_line(len - 1);
    let first = lines.start.saturating_sub(1).min(last);
    let end = lines.end.saturating_sub(2).max(first).min(last);
    text.line_to_char(first)..text.line_to_char(end + 1)
}

/// Anchor at the terminating newline of `range` (before a `\r\n` pair), or at
/// EOF for an unterminated final line and empty documents.
pub fn anchor(text: &Rope, range: &Range<usize>) -> usize {
    let len = text.len_chars();
    if range.end == 0 || range.end > len {
        return range.end.min(len);
    }
    let mut anchor = range.end - 1;
    if anchor > 0 && text.char(anchor) == '\n' && text.char(anchor - 1) == '\r' {
        anchor -= 1;
    }
    if range.end == len && !matches!(text.char(anchor), '\n' | '\r') {
        anchor = range.end;
    }
    anchor
}

/// Only map an unchanged, unique context window. A diff algorithm alone can
/// arbitrarily pair identical lines after deletions. We deliberately prefer an
/// unavailable location over that false precision. Exact documents need no search.
// Index line fingerprints once per snapshot. Hashes only select candidates;
// full byte comparisons prove each match, so collisions cannot mis-anchor code.
#[derive(Default)]
struct LineIndex(HashMap<u64, Vec<usize>>);
impl LineIndex {
    fn hash(bytes: impl Iterator<Item = u8>) -> u64 {
        bytes.fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
    }
    fn new(text: &Rope) -> Self {
        let mut index = Self::default();
        let mut byte = 0;
        for line in text.lines() {
            if line.len_bytes() == 0 {
                continue;
            }
            let hash = Self::hash(line.chunks().flat_map(str::bytes));
            let candidates = index.0.entry(hash).or_default();
            // Extremely repetitive windows remain available in the reader. A
            // bounded candidate list prevents pathological work during typing.
            if candidates.len() <= 64 {
                candidates.push(byte);
            }
            byte += line.len_bytes();
        }
        index
    }
    fn unique(&self, text: &str, needle: &str, offset: usize, hash: u64) -> Option<usize> {
        let candidates = self.0.get(&hash)?;
        if candidates.len() > 64 {
            return None;
        }
        let mut found = None;
        for candidate in candidates {
            let Some(start) = candidate.checked_sub(offset) else {
                continue;
            };
            if text.get(start..start + needle.len()) == Some(needle) {
                if found.is_some() {
                    return None;
                }
                found = Some(start);
            }
        }
        found
    }
}

pub struct LineMapper<'a> {
    source: &'a str,
    before: Rope,
    local: &'a Rope,
    after: String,
    equal: bool,
    before_index: LineIndex,
    after_index: LineIndex,
    /// Computed on the first discussion that cannot be mapped exactly.
    hunks: OnceCell<Vec<Hunk>>,
}

impl<'a> LineMapper<'a> {
    pub fn new(source: &'a str, local: &'a Rope) -> Option<Self> {
        if source.len() > 2 * 1024 * 1024 || local.len_bytes() > 2 * 1024 * 1024 {
            return None;
        }
        let before = Rope::from_str(source);
        let equal = &before == local;
        let (before_index, after_index) = if equal {
            (LineIndex::default(), LineIndex::default())
        } else {
            (LineIndex::new(&before), LineIndex::new(local))
        };
        Some(Self {
            before_index,
            after_index,
            source,
            before,
            local,
            after: if equal {
                String::new()
            } else {
                local.to_string()
            },
            equal,
            hunks: OnceCell::new(),
        })
    }

    /// Place one-based `lines` of the snapshot: exact when the unchanged context
    /// can be verified, else carried through a line diff of the snapshot and the
    /// document, else at the clamped original line numbers.
    pub fn place(&self, lines: Range<usize>) -> (Range<usize>, Placement) {
        if let Some(range) = self.map(lines.clone()) {
            return (range, Placement::Exact);
        }
        if self.equal
            || lines.start == 0
            || lines.start >= lines.end
            || lines.end - 1 > self.before.len_lines()
        {
            return (line_range(self.local, lines), Placement::Approximate);
        }
        let hunks = self
            .hunks
            .get_or_init(|| line_hunks(self.source, &self.after));
        let (_, lines) = track(lines, hunks);
        (line_range(self.local, lines), Placement::Changed)
    }

    pub fn map(&self, lines: Range<usize>) -> Option<Range<usize>> {
        if lines.start == 0 || lines.start >= lines.end || lines.end - 1 > self.before.len_lines() {
            return None;
        }
        let start = self.before.try_line_to_char(lines.start - 1).ok()?;
        let end = self.before.try_line_to_char(lines.end - 1).ok()?;
        if start == end {
            return None;
        }
        if self.equal {
            return Some(start..end);
        }
        let window_start_line = (lines.start - 1).saturating_sub(3);
        let window_end_line = (lines.end - 1 + 3).min(self.before.len_lines());
        let window_start = self.before.line_to_char(window_start_line);
        let window_end = self.before.line_to_char(window_end_line);
        let needle = self.before.slice(window_start..window_end).to_string();
        // Choose the rarest line in the whole window, then verify the complete
        // context at every candidate. Overlapping occurrences are included.
        let mut offset = 0;
        let mut rarest = None;
        for line in self.before.slice(window_start..window_end).lines() {
            if line.len_bytes() == 0 {
                continue;
            }
            let hash = LineIndex::hash(line.chunks().flat_map(str::bytes));
            let count = self
                .before_index
                .0
                .get(&hash)?
                .len()
                .max(self.after_index.0.get(&hash)?.len());
            if rarest.is_none_or(|(_, _, best)| count < best) {
                rarest = Some((offset, hash, count));
            }
            offset += line.len_bytes();
        }
        let (offset, hash, _) = rarest?;
        self.before_index
            .unique(self.source, &needle, offset, hash)?;
        let byte = self
            .after_index
            .unique(&self.after, &needle, offset, hash)?;
        // An unterminated final source line must not match a prefix of a changed
        // local line ("four" -> "fourChanged"). Preserve the EOF boundary.
        if window_end == self.before.len_chars()
            && !self.source.ends_with('\n')
            && byte + needle.len() != self.after.len()
        {
            return None;
        }
        let base = self.local.byte_to_char(byte);
        // Never map a code line to a substring inside another line.
        if self.local.line_to_char(self.local.char_to_line(base)) != base {
            return None;
        }
        Some(base + start - window_start..base + end - window_start)
    }
}

#[cfg(test)]
fn map_lines(source: &str, local: &Rope, lines: Range<usize>) -> Option<Range<usize>> {
    LineMapper::new(source, local)?.map(lines)
}

/// Strip terminal controls (including bidi overrides) before any UI consumes
/// remote content. Preserve paragraphs; tabs become spaces. Rendering is plain text.
pub fn safe_text(text: &str) -> String {
    text.chars().filter_map(|c| match c {
        '\n' => Some(c),
        '\t' => Some(' '),
        c if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => None,
        c => Some(c),
    }).collect()
}

/// Identical wrapping is used by layout and drawing, including very narrow views
/// and wide Unicode graphemes. Inline expansion is bounded; review-open holds all text.
pub fn rows(block: &Block, width: u16) -> Vec<String> {
    let width = usize::from(width.saturating_sub(2)).max(1);
    let limit = if block.expanded { 40 } else { 3 };
    let mut rows = Vec::new();
    // Stop scanning and allocating once the inline budget is exhausted, even
    // for very large conversations. The complete text is retained in Thread.
    fn append(text: &str, width: usize, limit: usize, rows: &mut Vec<String>) -> bool {
        for line in text.lines() {
            if rows.len() >= limit {
                return false;
            }
            let mut row = String::new();
            let mut used = 0;
            for g in line.graphemes(true) {
                let w = g.width();
                if used + w > width && !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                    if rows.len() >= limit {
                        return false;
                    }
                }
                if w <= width {
                    row.push_str(g);
                    used += w;
                }
            }
            rows.push(row);
        }
        true
    }
    let placement = block.placement.label();
    let title = format!(
        "{} {}{}{placement}",
        if block.expanded { "[-]" } else { "[+]" },
        block.thread.title(),
        if placement.is_empty() { "" } else { " · " },
    );
    let mut complete = append(&title, width, limit, &mut rows);
    if complete && block.expanded {
        for comment in &block.thread.comments {
            complete = append(&format!("@{}", comment.author), width, limit, &mut rows)
                && append(&comment.body, width, limit, &mut rows);
            if !complete {
                break;
            }
        }
    } else if complete {
        if let Some(comment) = block.thread.comments.first() {
            complete = append(
                comment.body.lines().next().unwrap_or(""),
                width,
                limit,
                &mut rows,
            );
        }
    }
    if !complete {
        rows.push("… :review-open for full discussion".into());
    }
    rows
}

/// Shared accumulator ensures the formatter reserves exactly the rows drawn by
/// the terminal decoration. Multiple threads can share one anchor.
pub struct Layout<'a> {
    blocks: &'a [Block],
    width: u16,
    next: usize,
    pending: Range<usize>,
}

impl<'a> Layout<'a> {
    pub fn new(blocks: &'a [Block], width: u16) -> Self {
        Self {
            blocks,
            width,
            next: 0,
            pending: 0..0,
        }
    }
    pub fn reset(&mut self, pos: usize) -> usize {
        self.next = self.blocks.partition_point(|b| b.anchor < pos);
        self.pending = self.next..self.next;
        self.next_anchor()
    }
    fn next_anchor(&self) -> usize {
        self.blocks.get(self.next).map_or(usize::MAX, |b| b.anchor)
    }
    pub fn anchor(&mut self) -> usize {
        self.next += 1;
        self.pending.end = self.next;
        self.next_anchor()
    }
    pub fn take_rows(&mut self) -> Vec<String> {
        let rows = self.blocks[self.pending.clone()]
            .iter()
            .flat_map(|b| rows(b, self.width))
            .collect();
        self.pending.start = self.pending.end;
        rows
    }
}
impl LineAnnotation for Layout<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.reset(pos)
    }
    fn process_anchor(&mut self, _: &FormattedGrapheme) -> usize {
        self.anchor()
    }
    fn insert_virtual_lines(&mut self, _: usize, _: Position, _: usize) -> Position {
        Position::new(self.take_rows().len(), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn map_revision_and_local_shifts() {
        let source = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        assert_eq!(map_lines(source, &Rope::from(source), 4..6), Some(14..24));
        let local = Rope::from(format!("unsaved\n{source}"));
        assert_eq!(map_lines(source, &local, 4..6), Some(22..32));
        assert_eq!(
            map_lines(source, &Rope::from(source.replace("four", "changed")), 4..6),
            None
        );
        assert_eq!(
            map_lines(source, &Rope::from(source.replace("four\n", "")), 4..6),
            None
        );
    }
    #[test]
    fn rejects_ambiguous_and_invalid_anchors() {
        let block = "a\nb\nc\nd\ne\nf\ng\n";
        let source = format!("{block}{block}");
        assert!(map_lines(&source, &Rope::from(format!("x\n{source}")), 4..5).is_none());
        assert!(map_lines(block, &Rope::from(block), 0..1).is_none());
        assert!(map_lines(block, &Rope::from(block), 100..101).is_none());
        assert!(map_lines(block, &Rope::from(format!("prefix{block}")), 4..5).is_none());
        assert!(map_lines(&"a\n".repeat(8), &Rope::from("a\n".repeat(7)), 4..5).is_none());
        assert!(map_lines(
            "one\ntwo\nthree\nfour",
            &Rope::from("one\ntwo\nthree\nfourChanged"),
            4..5
        )
        .is_none());
    }
    #[test]
    fn overlapping_repetition_and_unterminated_changed_line_are_unavailable() {
        assert!(map_lines(&"a\n".repeat(8), &Rope::from("a\n".repeat(7)), 4..5).is_none());
        assert!(map_lines(
            "one\ntwo\nthree\nfour",
            &Rope::from("one\ntwo\nthree\nfourChanged"),
            4..5
        )
        .is_none());
        assert_eq!(
            map_lines(
                "one\ntwo\nthree\nfour",
                &Rope::from("prefix\none\ntwo\nthree\nfour"),
                4..5
            ),
            Some(21..25)
        );
    }

    #[test]
    fn many_anchors_share_one_large_document_snapshot() {
        let source: String = (0..100_000).map(|i| format!("unique line {i}\n")).collect();
        let local = Rope::from(format!("unsaved\n{source}"));
        let start = std::time::Instant::now();
        let mapper = LineMapper::new(&source, &local).unwrap();
        for line in (1000..100_000).step_by(1000) {
            let mapped = mapper.map(line..line + 1).unwrap();
            assert_eq!(local.char_to_line(mapped.start), line);
        }
        eprintln!(
            "Mapped 99 discussions in a {}-byte modified document in {:?}",
            source.len(),
            start.elapsed()
        );
    }

    fn hunk(old_start: usize, old_len: usize, new_start: usize, new_len: usize) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
        }
    }

    #[test]
    fn tracking_follows_shifts_and_detects_touches() {
        // Lines 10-12. An edit above shifts the range without touching it.
        assert_eq!(track(10..13, &[hunk(2, 1, 2, 3)]), (false, 12..15));
        // Deletion above.
        assert_eq!(track(10..13, &[hunk(2, 2, 1, 0)]), (false, 8..11));
        // An edit below is unrelated.
        assert_eq!(track(10..13, &[hunk(20, 1, 20, 1)]), (false, 10..13));
        // Changing a discussed line.
        assert_eq!(track(10..13, &[hunk(11, 1, 11, 1)]), (true, 10..13));
        // Replacing the whole range with more lines widens it.
        assert_eq!(track(10..13, &[hunk(10, 3, 10, 5)]), (true, 10..15));
        // Inserting directly after the last line (a missing check) counts.
        assert_eq!(track(10..13, &[hunk(12, 0, 13, 2)]), (true, 10..15));
        // Inserting directly before the first line counts too.
        assert_eq!(track(10..13, &[hunk(9, 0, 10, 1)]), (true, 10..14));
        // Inserting further away does not.
        assert_eq!(track(10..13, &[hunk(13, 0, 14, 1)]), (false, 10..13));
        // Deleting the discussed lines keeps a one-line anchor.
        let (touched, range) = track(10..13, &[hunk(10, 3, 9, 0)]);
        assert!(touched);
        assert_eq!(range.len(), 1);
    }

    #[test]
    fn line_hunks_use_git_zero_context_coordinates() {
        let base = "a\nb\nc\n";
        assert_eq!(line_hunks(base, base), vec![]);
        assert_eq!(line_hunks(base, "a\nx\nb\nc\n"), vec![hunk(1, 0, 2, 1)]);
        assert_eq!(line_hunks(base, "a\nc\n"), vec![hunk(2, 1, 1, 0)]);
        assert_eq!(line_hunks(base, "a\nB\nc\n"), vec![hunk(2, 1, 2, 1)]);
        assert_eq!(line_hunks(base, "x\na\nb\nc\n"), vec![hunk(0, 0, 1, 1)]);
        assert_eq!(line_hunks(base, ""), vec![hunk(1, 3, 0, 0)]);
    }

    /// Place line `line` of `source` in `local`: the placement and the
    /// one-based local line the block's range starts on.
    fn place(source: &str, local: &str, line: usize) -> (Placement, usize) {
        let local = Rope::from(local);
        let (range, placement) = LineMapper::new(source, &local)
            .unwrap()
            .place(line..line + 1);
        assert!(range.end <= local.len_chars());
        (placement, local.char_to_line(range.start) + 1)
    }

    #[test]
    fn placement_follows_changed_code() {
        let source =
            "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n\nfn c() {\n    three();\n}\n";
        // Unchanged, and shifted by an unsaved edit: exact.
        assert_eq!(place(source, source, 6), (Placement::Exact, 6));
        let shifted = format!("// new\n{source}");
        assert_eq!(place(source, &shifted, 6), (Placement::Exact, 7));
        // The discussed line itself changed.
        let changed = source.replace("two();", "two(1);");
        assert_eq!(place(source, &changed, 6), (Placement::Changed, 6));
        // Only the context window changed.
        let context = source.replace("fn b() {", "fn b(x: u8) {");
        assert_eq!(place(source, &context, 6), (Placement::Changed, 6));
        // The discussed line was deleted: shown at the remaining line above it.
        let deleted = source.replace("    two();\n", "");
        assert_eq!(place(source, &deleted, 6), (Placement::Changed, 5));
        // A change combined with a shift.
        let both = format!("x\ny\nz\n{changed}");
        assert_eq!(place(source, &both, 6), (Placement::Changed, 9));
        // Undoing the change restores the exact location.
        assert_eq!(place(source, source, 10), (Placement::Exact, 10));
        // Code the exact mapper rejects as ambiguous follows the diff.
        let repeated = format!("{source}{source}");
        assert_eq!(
            place(&repeated, &format!("x\n{repeated}"), 17),
            (Placement::Changed, 18)
        );
        // Short, empty and unterminated documents stay in bounds.
        assert_eq!(place(source, "short\n", 10), (Placement::Changed, 1));
        assert_eq!(place(source, "", 10), (Placement::Changed, 1));
        assert_eq!(place(source, "fn a() {", 10).1, 1);
        // Lines outside the snapshot keep the original number, clamped.
        assert_eq!(place(source, &changed, 40), (Placement::Approximate, 11));
        assert_eq!(place(source, source, 40), (Placement::Approximate, 11));
    }

    #[test]
    fn ranges_and_anchors_in_short_and_empty_documents() {
        let empty = Rope::from("");
        assert_eq!(line_range(&empty, 3..4), 0..0);
        assert_eq!(anchor(&empty, &(0..0)), 0);
        let unterminated = Rope::from("a\nb");
        assert_eq!(line_range(&unterminated, 5..6), 2..3);
        assert_eq!(anchor(&unterminated, &(2..3)), 3);
        let terminated = Rope::from("a\nb\n");
        assert_eq!(line_range(&terminated, 9..10), 2..4);
        assert_eq!(line_range(&terminated, 1..3), 0..4);
        assert_eq!(anchor(&terminated, &(2..4)), 3);
        let crlf = Rope::from("a\r\nb\r\n");
        assert_eq!(line_range(&crlf, 1..2), 0..3);
        assert_eq!(anchor(&crlf, &(0..3)), 1);
    }

    #[test]
    fn block_titles_name_placement_and_github_state() {
        let thread = Arc::new(Thread {
            outdated: true,
            comments: vec![Comment {
                author: "a".into(),
                body: "body".into(),
            }],
            ..Default::default()
        });
        let block = |placement| Block {
            thread: thread.clone(),
            range: 0..0,
            anchor: 0,
            expanded: false,
            placement,
        };
        assert_eq!(
            rows(&block(Placement::Changed), 200)[0],
            "[+] @a · open · outdated · 1 comment(s) · code changed"
        );
        assert_eq!(
            rows(&block(Placement::Exact), 200)[0],
            "[+] @a · open · outdated · 1 comment(s)"
        );
        assert!(rows(&block(Placement::Approximate), 200)[0].ends_with("· approximate location"));
    }

    #[tokio::test]
    async fn invalidation_cancels_pending_work() {
        let pending = tokio::spawn(std::future::pending::<()>());
        let mut state = State {
            task: Some(pending.abort_handle()),
            ..State::default()
        };
        state.invalidate(None);
        assert!(pending.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn summary_and_indicator_explain_counts() {
        let thread = |id: &str, lines, resolved| {
            Arc::new(Thread {
                id: id.into(),
                path: "a.rs".into(),
                lines,
                location: String::new(),
                resolved,
                comments: vec![],
                diff: String::new(),
                url: String::new(),
                ..Default::default()
            })
        };
        let review = Review {
            label: "o/repo#12 @ abc".into(),
            threads: vec![
                thread("1", Some(1..2), false),
                thread("2", None, false),
                thread("3", Some(3..4), true),
            ],
            sources: HashMap::new(),
            ..Default::default()
        };
        assert_eq!(review.pull(), "o/repo#12");
        let summary = review.summary();
        assert!(summary.contains("3 discussion(s), 2 open"), "{summary}");
        assert!(
            summary.contains("2 inline (0 outdated), 1 old-side/file-level"),
            "{summary}"
        );
        let empty = Review {
            threads: vec![],
            ..review.clone()
        };
        assert_eq!(empty.summary(), "o/repo#12: no review discussions");

        let mut state = State::default();
        assert_eq!(state.indicator(), None);
        state.enabled = true;
        assert_eq!(state.indicator().unwrap(), "reviews: no PR");
        state.loading = true;
        assert_eq!(state.indicator().unwrap(), "reviews: loading");
        state.loading = false;
        state.failed = true;
        assert_eq!(state.indicator().unwrap(), "reviews: error");
        state.review = Some(Arc::new(review));
        assert_eq!(state.indicator().unwrap(), "repo#12 2/3 open");
        state.invalidate(None);
        assert!(!state.failed);
    }

    #[test]
    fn controls_and_context_generations() {
        assert_eq!(safe_text("a\x1b[31m\r\u{202e}b\t\nc"), "a[31mb \nc");
        let context = Context {
            root: "/r".into(),
            git_dir: "/r/.git".into(),
            branch: "main".into(),
            head: "abc".into(),
            config: Vec::new(),
        };
        let mut state = State {
            enabled: true,
            ..State::default()
        };
        state.invalidate(Some(context.clone()));
        let old = state.generation;
        assert!(state.accepts(old, &context));
        state.invalidate(Some(context.clone()));
        assert!(!state.accepts(old, &context));
        state.enabled = false;
        assert!(!state.accepts(state.generation, &context));
    }
}
