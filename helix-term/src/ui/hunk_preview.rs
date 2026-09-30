//! Popup contents showing what a change (a diff hunk) removed and added.

use std::{iter::Peekable, ops::Range, slice};

use helix_core::{
    diff::compare_words,
    line_ending::{get_line_ending, LineEnding},
    unicode::width::UnicodeWidthChar,
    Rope,
};
use helix_vcs::{DiffHandle, Hunk};
use helix_view::{
    graphics::{Margin, Modifier, Rect, Style},
    Theme,
};
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans, Text},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::compositor::{Component, Context};

/// Lines shown per side of a hunk; the rest are summarized.
const MAX_LINES: usize = 500;
/// Characters shown per line; the rest are cut.
const MAX_LINE_CHARS: usize = 1000;
/// Changed words are only marked in hunks up to this size, as every removed
/// line is compared with every added line.
const MAX_WORD_DIFF_LINES: usize = 100;
const MAX_WORD_DIFF_BYTES: usize = 16 * 1024;

/// Keeps changed whitespace visible (and away from the wrapping, which drops
/// spaces at the end of a row).
const CHANGED_SPACE: char = '·';
const CHANGED_TAB: char = '→';
const NBSP: char = '\u{a0}';
const CHANGED_CR: char = '␍';

#[derive(Debug, Default, PartialEq, Eq)]
struct Line {
    /// The line without its `\n`: the `\r` of a CRLF line break stays, so a
    /// change of line break is a change of the text.
    text: String,
    /// Whether the line was longer than [`MAX_LINE_CHARS`].
    cut: bool,
    /// The byte ranges of `text` that differ from the line it was paired
    /// with. Empty for a line that is all new, when words were not compared,
    /// or when the line was cut.
    changed: Vec<Range<usize>>,
}

/// One side of a hunk: the removed lines or the added lines.
#[derive(Debug, Default, PartialEq, Eq)]
struct Side {
    lines: Vec<Line>,
    /// Lines of the hunk beyond [`MAX_LINES`].
    omitted: usize,
    /// Whether the last line shown is the last line of the file and has no
    /// line break.
    no_newline_at_end: bool,
}

impl Side {
    /// The side showing `lines` (see [`file_lines`]) of `text`.
    fn new(text: &Rope, lines: Range<usize>) -> Self {
        let text = text.slice(..);
        let shown = lines.start..lines.end.min(lines.start + MAX_LINES);
        let lines_shown = shown
            .clone()
            .map(|idx| {
                let line = text.line(idx);
                let (line_break, cr) = match get_line_ending(&line) {
                    Some(LineEnding::Crlf) => (2, 1),
                    Some(line_ending) => (line_ending.len_chars(), 0),
                    None => (0, 0),
                };
                let len = line.len_chars() - line_break;
                let cut = len > MAX_LINE_CHARS;
                let kept = if cut { MAX_LINE_CHARS } else { len + cr };
                Line {
                    text: line.slice(..kept).to_string(),
                    cut,
                    changed: Vec::new(),
                }
            })
            .collect();
        let last_is_shown = !shown.is_empty() && shown.end == lines.end;
        Self {
            lines: lines_shown,
            omitted: lines.end - shown.end,
            no_newline_at_end: last_is_shown
                && shown.end == text.len_lines()
                && get_line_ending(&text.line(shown.end - 1)).is_none(),
        }
    }
}

/// The part of `lines` that are lines of the file. The diff also counts the
/// empty line that a [`Rope`] has after a final line break (or when empty),
/// which `git diff` does not.
fn file_lines(text: &Rope, lines: Range<u32>) -> Range<usize> {
    let len = text.len_lines();
    let file_len = if text.line(len - 1).len_chars() == 0 {
        len - 1
    } else {
        len
    };
    let end = (lines.end as usize).min(file_len);
    (lines.start as usize).min(end)..end
}

/// The diff of one hunk, shown as a titled section of the popup.
#[derive(Debug, PartialEq, Eq)]
pub struct HunkSection {
    title: &'static str,
    /// Position among the file's hunks, counted from 1.
    index: u32,
    total: u32,
    /// `@@ -a,b +c,d @@`, numbered like `git diff`.
    location: String,
    removed: Side,
    added: Side,
    tab_width: usize,
}

impl HunkSection {
    /// The section for `hunk`, the `index`th (from 0) of `total` hunks between
    /// `base` and `doc`.
    pub fn new(
        title: &'static str,
        base: &Rope,
        doc: &Rope,
        hunk: &Hunk,
        index: u32,
        total: u32,
        tab_width: usize,
    ) -> Self {
        let before = file_lines(base, hunk.before.clone());
        let after = file_lines(doc, hunk.after.clone());
        let mut removed = Side::new(base, before.clone());
        let mut added = Side::new(doc, after.clone());
        mark_changed_words(&mut removed, &mut added);
        Self {
            title,
            index: index + 1,
            total,
            location: location(before, after),
            removed,
            added,
            tab_width: tab_width.max(1),
        }
    }

    /// The section for the hunk of `handle` that contains `line`, or that
    /// removed lines just above it, as marked in the diff gutter.
    pub fn at_line(
        title: &'static str,
        handle: &DiffHandle,
        line: u32,
        tab_width: usize,
    ) -> Option<Self> {
        let diff = handle.load();
        let index = diff.hunk_at(line, true)?;
        Some(Self::new(
            title,
            diff.diff_base(),
            diff.doc(),
            &diff.nth_hunk(index),
            index,
            diff.len(),
            tab_width,
        ))
    }

    /// Whether both sections show the same change, whatever their titles.
    pub fn same_change(&self, other: &Self) -> bool {
        self.location == other.location
            && self.removed == other.removed
            && self.added == other.added
    }

    fn render(&self, theme: Option<&Theme>, lines: &mut Vec<Spans<'static>>) {
        let get = |scope: &str| theme.map(|theme| theme.get(scope)).unwrap_or_default();
        let title = get("ui.text").add_modifier(Modifier::BOLD);
        let meta = get("ui.text.inactive");

        lines.push(Spans::from(vec![
            Span::styled(
                format!("{} {}/{}", self.title, self.index, self.total),
                title,
            ),
            Span::styled(format!("  {}", self.location), meta),
        ]));
        for (side, sign, style) in [
            (&self.removed, "-", get("diff.minus")),
            (&self.added, "+", get("diff.plus")),
        ] {
            for line in &side.lines {
                lines.push(diff_line(sign, line, style, meta, self.tab_width));
            }
            if side.omitted > 0 {
                lines.push(Spans::from(Span::styled(
                    format!("{sign} … {} more lines", side.omitted),
                    meta,
                )));
            }
            if side.no_newline_at_end {
                lines.push(Spans::from(Span::styled(
                    "\\ No newline at end of file",
                    meta,
                )));
            }
        }
    }
}

/// Popup contents showing the diff of the hunks at the cursor.
pub struct HunkPreview {
    sections: Vec<HunkSection>,
    /// The size last required, and the text width it was measured for.
    size: Option<(u16, (u16, u16))>,
}

impl HunkPreview {
    pub fn new(sections: Vec<HunkSection>) -> Self {
        Self {
            sections,
            size: None,
        }
    }

    fn text(&self, theme: Option<&Theme>) -> Text<'static> {
        let mut lines = Vec::new();
        for (idx, section) in self.sections.iter().enumerate() {
            if idx > 0 {
                lines.push(Spans::default());
            }
            section.render(theme, &mut lines);
        }
        Text::from(lines)
    }
}

impl Component for HunkPreview {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let text = self.text(Some(&cx.editor.theme));
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0))
            .render(area.inner(Margin::all(1)), surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let padding = 2;
        let max_text_width = viewport.0.saturating_sub(padding).min(120);
        if let Some((text_width, size)) = self.size {
            if text_width == max_text_width {
                return Some(size);
            }
        }

        let text = self.text(None);
        // Measure with the wrapping used to render, line by line: `Paragraph`
        // counts rows in a `u16`, which a whole hunk can exceed.
        let measure = |width: u16| {
            text.lines.iter().fold((0, 0), |(widest, rows), line| {
                let line = Text::from(line.clone());
                let (line_width, line_rows) = Paragraph::new(&line)
                    .wrap(Wrap { trim: false })
                    .required_size(width);
                (line_width.max(widest), rows + usize::from(line_rows))
            })
        };
        let (width, _) = measure(max_text_width);
        // The text is rendered at the width of its widest row, where rows can
        // break differently, e.g. within a run of spaces.
        let (_, rows) = measure(width);
        // Leave room for the popup's border.
        let height = rows.min(usize::from(u16::MAX - 2 * padding)) as u16;
        let size = (width + padding, height + padding);
        self.size = Some((max_text_width, size));
        Some(size)
    }
}

/// One `-` or `+` line, with its changed words in reverse video. Changes
/// separated only by whitespace are shown as one, the whitespace in between
/// drawn as it is.
fn diff_line(
    sign: &str,
    line: &Line,
    style: Style,
    meta: Style,
    tab_width: usize,
) -> Spans<'static> {
    let emphasis = style.add_modifier(Modifier::REVERSED);
    let mut spans = vec![Span::styled(format!("{sign} "), style)];
    let mut shown = String::new();
    let mut shown_emphasized = false;
    let mut column = 0;
    let joined = join_changes(&line.text, &line.changed);
    let mut joined = joined.iter().peekable();
    let mut changed_ranges = line.changed.iter().peekable();
    for (idx, ch) in line.text.char_indices() {
        let changed = advance_to(&mut changed_ranges, idx);
        let emphasized = advance_to(&mut joined, idx);
        if emphasized != shown_emphasized && !shown.is_empty() {
            let style = if shown_emphasized { emphasis } else { style };
            spans.push(Span::styled(std::mem::take(&mut shown), style));
        }
        shown_emphasized = emphasized;

        match ch {
            '\t' => {
                let width = tab_width - column % tab_width;
                if changed {
                    shown.push(CHANGED_TAB);
                    shown.extend(std::iter::repeat_n(NBSP, width - 1));
                } else {
                    shown.extend(std::iter::repeat_n(' ', width));
                }
                column += width;
            }
            // The `\r` of a CRLF line break.
            '\r' if idx + 1 == line.text.len() && !line.cut => {
                if changed {
                    shown.push(CHANGED_CR);
                    column += 1;
                }
            }
            ch if ch.is_control() => {
                shown.push(control_picture(ch));
                column += 1;
            }
            ch if changed && ch.is_whitespace() => {
                let width = ch.width().unwrap_or(1).max(1);
                shown.push(CHANGED_SPACE);
                shown.extend(std::iter::repeat_n(NBSP, width - 1));
                column += width;
            }
            ch => {
                shown.push(ch);
                column += ch.width().unwrap_or(0);
            }
        }
    }
    if !shown.is_empty() {
        let style = if shown_emphasized { emphasis } else { style };
        spans.push(Span::styled(shown, style));
    }
    if line.cut {
        spans.push(Span::styled("…", meta));
    }
    Spans::from(spans)
}

/// Skips the ascending `ranges` that end before `idx`, and returns whether the
/// next one contains it.
fn advance_to(ranges: &mut Peekable<slice::Iter<Range<usize>>>, idx: usize) -> bool {
    while ranges.next_if(|range| range.end <= idx).is_some() {}
    ranges.peek().is_some_and(|range| range.contains(&idx))
}

/// A visible stand-in for a control character, which would otherwise reach
/// the terminal as is.
fn control_picture(ch: char) -> char {
    match ch as u32 {
        code @ 0..=0x1f => char::from_u32(0x2400 + code).unwrap_or('\u{fffd}'),
        0x7f => '\u{2421}',
        _ => '\u{fffd}',
    }
}

/// The `@@ -a,b +c,d @@` header of a hunk, numbered like `git diff`: from 1,
/// the count left out when it is 1, and an empty side numbered by the line
/// before it.
fn location(before: Range<usize>, after: Range<usize>) -> String {
    fn side(lines: Range<usize>) -> String {
        match lines.len() {
            0 => format!("{},0", lines.start),
            1 => format!("{}", lines.start + 1),
            len => format!("{},{len}", lines.start + 1),
        }
    }
    format!("@@ -{} +{} @@", side(before), side(after))
}

/// Pairs each removed line with the added line it most likely became, and
/// marks the words that differ between them.
fn mark_changed_words(removed: &mut Side, added: &mut Side) {
    let (rows, cols) = (removed.lines.len(), added.lines.len());
    let bytes: usize = (removed.lines.iter())
        .chain(&added.lines)
        .map(|line| line.text.len())
        .sum();
    if rows == 0
        || cols == 0
        || removed.omitted + added.omitted > 0
        || rows + cols > MAX_WORD_DIFF_LINES
        || bytes > MAX_WORD_DIFF_BYTES
    {
        return;
    }

    // Two lines can pair when they share a third of the longer one's
    // visible text. The weight prefers pairs that share more. Cut lines do
    // not pair: what differs may be past the cut.
    let mut pairs = Vec::with_capacity(rows * cols);
    for old in &removed.lines {
        for new in &added.lines {
            if old.cut || new.cut {
                pairs.push(None);
                continue;
            }
            let (old_changed, new_changed) = compare_words(&old.text, &new.text);
            let kept = visible(&old.text) - visible_in(&old.text, &old_changed);
            let longest = visible(&old.text).max(visible(&new.text));
            pairs.push((kept * 3 >= longest).then_some((kept + 1, old_changed, new_changed)));
        }
    }
    let weight = |row: usize, col: usize| pairs[row * cols + col].as_ref().map_or(0, |pair| pair.0);

    // The in-order pairing with the most weight, like a longest common
    // subsequence.
    let mut best = vec![0; (rows + 1) * (cols + 1)];
    let at = |row: usize, col: usize| row * (cols + 1) + col;
    for row in 1..=rows {
        for col in 1..=cols {
            let paired = match weight(row - 1, col - 1) {
                0 => 0,
                weight => best[at(row - 1, col - 1)] + weight,
            };
            best[at(row, col)] = paired
                .max(best[at(row - 1, col)])
                .max(best[at(row, col - 1)]);
        }
    }
    // Walk back, leaving later lines unpaired where that is as good, so a
    // changed line followed by new lines pairs with the first of them.
    let (mut row, mut col) = (rows, cols);
    while row > 0 && col > 0 {
        let score = best[at(row, col)];
        if score == best[at(row, col - 1)] {
            col -= 1;
        } else if score == best[at(row - 1, col)] {
            row -= 1;
        } else {
            row -= 1;
            col -= 1;
            if let Some((_, old_changed, new_changed)) = pairs[row * cols + col].take() {
                let old = &mut removed.lines[row];
                old.changed = worth_marking(&old.text, old_changed);
                let new = &mut added.lines[col];
                new.changed = worth_marking(&new.text, new_changed);
            }
        }
    }
}

fn visible(text: &str) -> usize {
    text.chars().filter(|ch| !ch.is_whitespace()).count()
}

fn visible_in(text: &str, ranges: &[Range<usize>]) -> usize {
    ranges
        .iter()
        .map(|range| visible(&text[range.clone()]))
        .sum()
}

/// The changes of a line, or none when the line has too little unchanged text
/// for them to stand out.
fn worth_marking(line: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let total = visible(line);
    let changed = visible_in(line, &ranges);
    // At least a third of the visible text must be unchanged. Changes of
    // whitespace alone are always kept: they would not be seen otherwise.
    if total > 0 && (changed == total || changed * 3 > total * 2) {
        return Vec::new();
    }
    ranges
}

/// `ranges` with those separated only by whitespace joined.
fn join_changes(line: &str, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut joined: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match joined.last_mut() {
            Some(last) if line[last.end..range.start].chars().all(char::is_whitespace) => {
                last.end = range.end
            }
            _ => joined.push(range.clone()),
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(before: Range<u32>, after: Range<u32>) -> Hunk {
        Hunk { before, after }
    }

    fn section(base: &str, doc: &str, hunk: Hunk) -> HunkSection {
        HunkSection::new(
            "Change",
            &Rope::from(base),
            &Rope::from(doc),
            &hunk,
            0,
            1,
            4,
        )
    }

    fn texts(side: &Side) -> Vec<&str> {
        side.lines.iter().map(|line| line.text.as_str()).collect()
    }

    /// The changed text of each line.
    fn changed_text(side: &Side) -> Vec<Vec<&str>> {
        side.lines
            .iter()
            .map(|line| {
                (line.changed.iter())
                    .map(|range| &line.text[range.clone()])
                    .collect()
            })
            .collect()
    }

    /// The rendered rows, without styles.
    fn rendered(preview: &HunkPreview) -> Vec<String> {
        preview.text(None).lines.iter().map(String::from).collect()
    }

    fn preview(base: &str, doc: &str, hunk: Hunk) -> Vec<String> {
        rendered(&HunkPreview::new(vec![section(base, doc, hunk)]))
    }

    /// The text of the cells drawn in reverse video on each row, as rendered
    /// 40 columns wide.
    fn reversed(base: &str, doc: &str, hunk: Hunk) -> Vec<String> {
        let text = HunkPreview::new(vec![section(base, doc, hunk)]).text(None);
        let area = Rect::new(0, 0, 40, text.lines.len() as u16);
        let mut surface = Surface::empty(area);
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .render(area, &mut surface);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| &surface[(x, y)])
                    .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
                    .map(|cell| cell.symbol.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn location_is_numbered_like_git() {
        assert_eq!(location(1322..1324, 1322..1325), "@@ -1323,2 +1323,3 @@");
        assert_eq!(location(4..4, 4..5), "@@ -4,0 +5 @@");
        assert_eq!(location(4..6, 4..4), "@@ -5,2 +4,0 @@");
        assert_eq!(location(0..1, 0..1), "@@ -1 +1 @@");
    }

    #[test]
    fn pure_insertion_shows_the_added_lines() {
        let section = section(
            "deploy:\n  branch: x\n\nnext\n",
            "deploy:\n  branch: x\n  flags: \"--timeout=600\"\n\nnext\n",
            hunk(2..2, 2..3),
        );
        assert!(section.removed.lines.is_empty());
        assert_eq!(texts(&section.added), ["  flags: \"--timeout=600\""]);
        assert_eq!(changed_text(&section.added), [Vec::<&str>::new()]);
        assert_eq!(section.location, "@@ -2,0 +3 @@");
    }

    #[test]
    fn pure_removal_shows_the_removed_lines() {
        let section = section("a\nb\nc\n", "a\nc\n", hunk(1..2, 1..1));
        assert_eq!(texts(&section.removed), ["b"]);
        assert!(section.added.lines.is_empty());
    }

    #[test]
    fn modification_marks_changed_words() {
        let section = section(
            "  flags: \"--timeout=300\"\r\n",
            "  flags: \"--timeout=600\"\r\n",
            hunk(0..1, 0..1),
        );
        assert_eq!(changed_text(&section.removed), [["300"]]);
        assert_eq!(changed_text(&section.added), [["600"]]);
        assert_eq!(
            rendered(&HunkPreview::new(vec![section])),
            [
                "Change 1/1  @@ -1 +1 @@",
                "-   flags: \"--timeout=300\"",
                "+   flags: \"--timeout=600\"",
            ]
        );
    }

    #[test]
    fn changed_lines_pair_with_the_lines_they_became() {
        let section = section(
            "let a = 1;\nlet b = 2;\n",
            "let a = 10;\nlet b = 2;\nlet c = 3;\n",
            hunk(0..2, 0..3),
        );
        assert_eq!(changed_text(&section.removed), [vec!["1"], vec![]]);
        // The new third line is all new: nothing in it is marked.
        assert_eq!(changed_text(&section.added), [vec!["10"], vec![], vec![]]);
    }

    #[test]
    fn a_changed_line_stays_apart_from_a_line_added_after_it() {
        let added = section("a(1, ctx)\n", "a(2, ctx)\nb(3, ctx)\n", hunk(0..1, 0..2));
        assert_eq!(changed_text(&added.removed), [vec!["1"]]);
        assert_eq!(changed_text(&added.added), [vec!["2"], vec![]]);

        let removed = section("a(2, ctx)\nb(3, ctx)\n", "a(1, ctx)\n", hunk(0..2, 0..1));
        assert_eq!(changed_text(&removed.removed), [vec!["2"], vec![]]);
        assert_eq!(changed_text(&removed.added), [vec!["1"]]);

        let alike = section("x = f(a)\n", "y = f(a)\nz = f(a)\n", hunk(0..1, 0..2));
        assert_eq!(changed_text(&alike.added), [vec!["y"], vec![]]);
    }

    #[test]
    fn a_changed_line_pairs_after_added_lines() {
        let section = section(
            "timeout: 30\n",
            "retries: 3\nbackoff: 1s\ntimeout: 60\n",
            hunk(0..1, 0..3),
        );
        assert_eq!(changed_text(&section.removed), [vec!["30"]]);
        assert_eq!(changed_text(&section.added), [vec![], vec![], vec!["60"]]);
    }

    #[test]
    fn lines_that_share_little_are_not_marked() {
        let section = section("foo(bar)\n", "baz(qux)\n", hunk(0..1, 0..1));
        assert_eq!(changed_text(&section.removed), [Vec::<&str>::new()]);
        assert_eq!(changed_text(&section.added), [Vec::<&str>::new()]);
    }

    #[test]
    fn changes_separated_by_whitespace_are_joined() {
        let section = section(
            "the quick brown fox\n",
            "the slow red fox\n",
            hunk(0..1, 0..1),
        );
        assert_eq!(changed_text(&section.removed), [["quick", "brown"]]);
        // The unchanged space in between is reversed too, but not drawn as a
        // changed space.
        assert_eq!(
            reversed(
                "the quick brown fox\n",
                "the slow red fox\n",
                hunk(0..1, 0..1)
            )[1..],
            ["quick brown", "slow red"]
        );
        assert_eq!(
            preview(
                "the quick brown fox\n",
                "the slow red fox\n",
                hunk(0..1, 0..1)
            )[1..],
            ["- the quick brown fox", "+ the slow red fox"]
        );
    }

    #[test]
    fn changed_whitespace_is_visible() {
        assert_eq!(
            preview("x = 1\n", "x = 1  \n", hunk(0..1, 0..1))[1..],
            ["- x = 1", "+ x = 1··"]
        );
        // Tabs are compared as tabs, not as the spaces they are shown as.
        assert_eq!(
            preview("\tfoo()\n", "    foo()\n", hunk(0..1, 0..1))[1..],
            ["- →\u{a0}\u{a0}\u{a0}foo()", "+ ····foo()"]
        );
        assert_eq!(
            preview("a\n    \nb\n", "a\n\nb\n", hunk(1..2, 1..2))[1..],
            ["- ····", "+ "]
        );
        assert_eq!(
            preview("a\r\nb\r\n", "a\nb\r\n", hunk(0..1, 0..1))[1..],
            ["- a␍", "+ a"]
        );
    }

    #[test]
    fn changes_render_in_reverse_video_up_to_the_end_of_the_row() {
        assert_eq!(
            reversed("x = 1\n", "x = 1  \n", hunk(0..1, 0..1))[1..],
            ["", "··"]
        );
        // Wide whitespace keeps its width.
        assert_eq!(
            reversed("x = 1\n", "x = 1\u{3000}\n", hunk(0..1, 0..1))[2],
            "·\u{a0}"
        );
    }

    #[test]
    fn crlf_line_breaks_stay_apart_from_trailing_whitespace() {
        assert_eq!(
            preview("let x = 1;  \r\n", "let x = 1;\r\n", hunk(0..1, 0..1))[1..],
            ["- let x = 1;··", "+ let x = 1;"]
        );
    }

    #[test]
    fn a_crlf_line_of_the_longest_shown_length_is_not_cut() {
        let line = "x".repeat(MAX_LINE_CHARS);
        let lines = preview(
            &format!("{line}\r\n"),
            &format!("{line}\n"),
            hunk(0..1, 0..1),
        );
        assert_eq!(lines[1], format!("- {line}␍"));
        assert_eq!(lines[2], format!("+ {line}"));
    }

    #[test]
    fn cut_lines_are_not_word_diffed() {
        let rest = "x".repeat(MAX_LINE_CHARS);
        let section = section(
            &format!("a {rest} end\n"),
            &format!("b {rest} end\n"),
            hunk(0..1, 0..1),
        );
        assert!(section.removed.lines[0].cut);
        assert_eq!(changed_text(&section.removed), [Vec::<&str>::new()]);
        assert_eq!(changed_text(&section.added), [Vec::<&str>::new()]);
    }

    #[test]
    fn unchanged_tabs_are_expanded_to_tab_stops() {
        assert_eq!(
            preview("\tx = 1\n", "\tx = 2\n", hunk(0..1, 0..1))[1..],
            ["-     x = 1", "+     x = 2"]
        );
        assert_eq!(
            preview("ab\tc\n", "ab\td\n", hunk(0..1, 0..1))[1..],
            ["- ab  c", "+ ab  d"]
        );
    }

    #[test]
    fn a_missing_final_newline_is_noted_like_git() {
        // Saving adds the final newline.
        let lines = preview("a\nb", "a\nb\n", hunk(1..2, 1..3));
        assert_eq!(
            lines,
            [
                "Change 1/1  @@ -2 +2 @@",
                "- b",
                "\\ No newline at end of file",
                "+ b",
            ]
        );
        assert_eq!(
            preview("a\n", "a\nb", hunk(1..2, 1..2)),
            [
                "Change 1/1  @@ -1,0 +2 @@",
                "+ b",
                "\\ No newline at end of file"
            ]
        );
        assert_eq!(
            preview("", "a\n", hunk(0..1, 0..2)),
            ["Change 1/1  @@ -0,0 +1 @@", "+ a"]
        );
    }

    #[test]
    fn control_characters_are_shown_as_pictures() {
        assert_eq!(
            preview("x\n", "x\x1b[0m\x08\x7f\n", hunk(0..1, 0..1))[2],
            "+ x␛[0m␈␡"
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "a".repeat(MAX_LINE_CHARS + 10);
        let section = section("", &format!("{long}\n"), hunk(0..0, 0..1));
        assert_eq!(section.added.lines[0].text.len(), MAX_LINE_CHARS);
        assert!(section.added.lines[0].cut);
        let lines = rendered(&HunkPreview::new(vec![section]));
        assert!(lines[1].ends_with("a…"));
    }

    #[test]
    fn long_hunks_are_cut_and_not_word_diffed() {
        let doc: String = (0..MAX_LINES + 5).map(|n| format!("line {n}\n")).collect();
        let section = section("", &doc, hunk(0..0, 0..(MAX_LINES as u32 + 5)));
        assert_eq!(section.added.lines.len(), MAX_LINES);
        assert_eq!(section.added.omitted, 5);
        assert!(!section.added.no_newline_at_end);
    }

    #[test]
    fn hunk_ranges_past_the_text_are_clamped() {
        let section = section("a\n", "a\nb", hunk(1..9, 1..9));
        assert!(section.removed.lines.is_empty());
        assert_eq!(texts(&section.added), ["b"]);
    }

    #[test]
    fn required_size_matches_the_wrapped_rows() {
        let words = ["p".repeat(60), "q".repeat(60), "r".repeat(60)].join(" ");
        let mut preview = HunkPreview::new(vec![section(
            &format!("\tx := \"{words}\"\n"),
            &format!("\ty := \"{words}\"\n"),
            hunk(0..1, 0..1),
        )]);
        let (width, height) = preview.required_size((120, 26)).unwrap();
        let text = preview.text(None);
        let rows = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .required_size(width - 2)
            .1;
        assert_eq!(height, rows + 2);
        // Both long lines wrap onto more rows than their width suggests.
        assert!(rows > 5, "{rows} rows");
    }

    #[test]
    fn required_size_of_huge_hunks_does_not_overflow() {
        let line = format!("{}\n", "word ".repeat(MAX_LINE_CHARS / 5));
        let doc = line.repeat(MAX_LINES);
        let mut preview =
            HunkPreview::new(vec![section("", &doc, hunk(0..0, 0..MAX_LINES as u32))]);
        // More rows than a `u16` holds in total, but counted line by line.
        let (_, height) = preview.required_size((20, 26)).unwrap();
        assert!(height > 20_000 && height < u16::MAX - 2, "{height}");
        // Room is left for the popup's border.
        assert_eq!(preview.required_size((3, 26)), Some((3, u16::MAX - 2)));
    }

    #[test]
    fn required_size_counts_rows_at_the_width_rendered() {
        // At the widest row's width, the second space starts a row of its own.
        let doc = format!("{}  {}\n", "a".repeat(58), "b".repeat(60));
        let mut preview = HunkPreview::new(vec![section("", &doc, hunk(0..0, 0..1))]);
        let (width, height) = preview.required_size((120, 26)).unwrap();
        let text = preview.text(None);
        let rows = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .required_size(width - 2)
            .1;
        assert_eq!(height, rows + 2);
    }

    #[test]
    fn odd_texts_and_ranges_do_not_panic() {
        let texts = [
            "",
            "\n",
            "no newline",
            "a\r\nb\r\n",
            "\tx\n\t\ty\n",
            "größe = 1\n日本 語\n",
            "e\u{301}\u{200d}x\n\u{1f600}\r\n\x1b\r\r\n",
        ];
        for base in texts {
            for doc in texts {
                for before in (0..4).flat_map(|start| (start..5).map(move |end| start..end)) {
                    for after in (0..4).flat_map(|start| (start..5).map(move |end| start..end)) {
                        let mut preview =
                            HunkPreview::new(vec![section(base, doc, hunk(before.clone(), after))]);
                        preview.required_size((30, 10));
                    }
                }
            }
        }
    }

    #[test]
    fn same_change_ignores_titles() {
        let head = section("a\n", "b\n", hunk(0..1, 0..1));
        let mut branch = section("a\n", "b\n", hunk(0..1, 0..1));
        branch.title = "Branch change";
        assert!(head.same_change(&branch));
        assert!(!head.same_change(&section("a\n", "c\n", hunk(0..1, 0..1))));
    }

    #[test]
    fn render_marks_signs_and_omissions() {
        let doc: String = (0..MAX_LINES + 2).map(|n| format!("{n}\n")).collect();
        let lines = rendered(&HunkPreview::new(vec![
            section("x = 1\n", "x = 2\n", hunk(0..1, 0..1)),
            section("", &doc, hunk(0..0, 0..(MAX_LINES as u32 + 2))),
        ]));
        assert_eq!(lines[0], "Change 1/1  @@ -1 +1 @@");
        assert_eq!(lines[1], "- x = 1");
        assert_eq!(lines[2], "+ x = 2");
        assert_eq!(lines[3], "");
        assert_eq!(
            lines[4],
            format!("Change 1/1  @@ -0,0 +1,{} @@", MAX_LINES + 2)
        );
        assert_eq!(lines[5], "+ 0");
        assert_eq!(lines.last().unwrap(), "+ … 2 more lines");
    }
}
