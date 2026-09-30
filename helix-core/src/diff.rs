use std::ops::Range;
use std::time::Instant;

use imara_diff::{Algorithm, Diff, Hunk, IndentHeuristic, IndentLevel, InternedInput, Token};
use ropey::RopeSlice;

use crate::{ChangeSet, Rope, Tendril, Transaction};

struct ChangeSetBuilder<'a> {
    res: ChangeSet,
    after: RopeSlice<'a>,
    file: &'a InternedInput<RopeSlice<'a>>,
    current_hunk: InternedInput<char>,
    char_diff: Diff,
    pos: u32,
}

impl ChangeSetBuilder<'_> {
    fn process_hunk(&mut self, before: Range<u32>, after: Range<u32>) {
        let len = self.file.before[self.pos as usize..before.start as usize]
            .iter()
            .map(|&it| self.file.interner[it].len_chars())
            .sum();
        self.res.retain(len);
        self.pos = before.end;

        // do not perform diffs on large hunks
        let len_before = before.end - before.start;
        let len_after = after.end - after.start;

        // Pure insertions/removals do not require a character diff.
        // Very large changes are ignored because their character diff is expensive to compute
        // TODO adjust heuristic to detect large changes?
        if len_before == 0
            || len_after == 0
            || len_after > 5 * len_before
            || 5 * len_after < len_before && len_before > 10
            || len_before + len_after > 200
        {
            let remove = self.file.before[before.start as usize..before.end as usize]
                .iter()
                .map(|&it| self.file.interner[it].len_chars())
                .sum();
            self.res.delete(remove);
            let mut fragment = Tendril::new();
            if len_after > 500 {
                // copying a rope line by line is slower then copying the entire
                // rope. Use to_string for very large changes instead..
                if self.file.after.len() == after.end as usize {
                    if after.start == 0 {
                        fragment = self.after.to_string().into();
                    } else {
                        let start = self.after.line_to_char(after.start as usize);
                        fragment = self.after.slice(start..).to_string().into();
                    }
                } else if after.start == 0 {
                    let end = self.after.line_to_char(after.end as usize);
                    fragment = self.after.slice(..end).to_string().into();
                } else {
                    let start = self.after.line_to_char(after.start as usize);
                    let end = self.after.line_to_char(after.end as usize);
                    fragment = self.after.slice(start..end).to_string().into();
                }
            } else {
                for &line in &self.file.after[after.start as usize..after.end as usize] {
                    for chunk in self.file.interner[line].chunks() {
                        fragment.push_str(chunk)
                    }
                }
            };
            self.res.insert(fragment);
        } else {
            // for reasonably small hunks, generating a ChangeSet from char diff can save memory
            // TODO use a tokenizer (word diff?) for improved performance
            let hunk_before = self.file.before[before.start as usize..before.end as usize]
                .iter()
                .flat_map(|&it| self.file.interner[it].chars());
            let hunk_after = self.file.after[after.start as usize..after.end as usize]
                .iter()
                .flat_map(|&it| self.file.interner[it].chars());
            self.current_hunk.update_before(hunk_before);
            self.current_hunk.update_after(hunk_after);
            // the histogram heuristic does not work as well
            // for characters because the same characters often reoccur
            // use myer diff instead
            self.char_diff.compute_with(
                Algorithm::Myers,
                &self.current_hunk.before,
                &self.current_hunk.after,
                self.current_hunk.interner.num_tokens(),
            );
            let mut pos = 0;
            for Hunk { before, after } in self.char_diff.hunks() {
                self.res.retain((before.start - pos) as usize);
                self.res.delete(before.len());
                pos = before.end;

                let res = self.current_hunk.after[after.start as usize..after.end as usize]
                    .iter()
                    .map(|&token| self.current_hunk.interner[token])
                    .collect();

                self.res.insert(res);
            }
            self.res
                .retain(self.current_hunk.before.len() - pos as usize);
            // reuse allocations
            self.current_hunk.clear();
        }
    }

    fn finish(mut self) -> ChangeSet {
        let len = self.file.before[self.pos as usize..]
            .iter()
            .map(|&it| self.file.interner[it].len_chars())
            .sum();

        self.res.retain(len);
        self.res
    }
}

struct RopeLines<'a>(RopeSlice<'a>);

impl<'a> imara_diff::TokenSource for RopeLines<'a> {
    type Token = RopeSlice<'a>;
    type Tokenizer = ropey::iter::Lines<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.lines()
    }

    fn estimate_tokens(&self) -> u32 {
        // we can provide a perfect estimate which is very nice for performance
        self.0.len_lines() as u32
    }
}

/// Compares `old` and `new` to generate a [`Transaction`] describing
/// the steps required to get from `old` to `new`.
pub fn compare_ropes(before: &Rope, after: &Rope) -> Transaction {
    let start = Instant::now();
    let res = ChangeSet::with_capacity(32);
    let after = after.slice(..);
    let file = InternedInput::new(RopeLines(before.slice(..)), RopeLines(after));
    let mut builder = ChangeSetBuilder {
        res,
        file: &file,
        after,
        pos: 0,
        current_hunk: InternedInput::default(),
        char_diff: Diff::default(),
    };
    let mut diff = Diff::compute(Algorithm::Histogram, &file);
    diff.postprocess_with_heuristic(
        &file,
        IndentHeuristic::new(|token| IndentLevel::for_ascii_line(file.interner[token].bytes(), 4)),
    );
    for hunk in diff.hunks() {
        builder.process_hunk(hunk.before, hunk.after)
    }
    let res = builder.finish().into();

    log::debug!(
        "rope diff took {}s",
        Instant::now().duration_since(start).as_secs_f64()
    );
    res
}

/// Compares `before` and `after` word by word and returns the byte ranges of
/// the words removed from `before` and the words inserted into `after`, in
/// ascending order.
///
/// A word is a run of alphanumeric characters and `_`, or a run of whitespace
/// other than `\n`. Every other character, including `\n`, is a word of its own.
pub fn compare_words(before: &str, after: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let before_words = split_words(before);
    let after_words = split_words(after);
    let mut input = InternedInput::default();
    input.update_before(before_words.iter().map(|word| &before[word.clone()]));
    input.update_after(after_words.iter().map(|word| &after[word.clone()]));
    let newline = input.interner.intern("\n");
    // Like characters, short words reoccur often, which the histogram
    // heuristic handles poorly.
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    // A pure insertion or removal can often slide, for example an added line
    // that ends in the same word as the line before it. Place it to cover whole
    // lines where possible, and as late as possible otherwise.
    diff.postprocess_with_heuristic(
        &input,
        |words: &[Token], hunk: Range<u32>, earliest_end: u32| {
            let len = hunk.len() as u32;
            let score = |end: u32| {
                let (start, end) = ((end - len) as usize, end as usize);
                let starts_line =
                    start == 0 || words[start - 1] == newline || words[start] == newline;
                let ends_line =
                    end == words.len() || words[end] == newline || words[end - 1] == newline;
                2 * u8::from(starts_line) + u8::from(ends_line)
            };
            // `max_by_key` returns the last of equally good ends.
            (earliest_end..=hunk.end)
                .max_by_key(|&end| score(end))
                .unwrap_or(hunk.end)
        },
    );

    let byte_range = |words: &[Range<usize>], range: Range<u32>| {
        words[range.start as usize].start..words[range.end as usize - 1].end
    };
    let mut removed = Vec::new();
    let mut inserted = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            removed.push(byte_range(&before_words, hunk.before));
        }
        if !hunk.after.is_empty() {
            inserted.push(byte_range(&after_words, hunk.after));
        }
    }
    (removed, inserted)
}

/// Splits `text` into the byte ranges of its words (see [`compare_words`]).
fn split_words(text: &str) -> Vec<Range<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |ch: char| {
        if ch.is_alphanumeric() || ch == '_' {
            Class::Word
        } else if ch.is_whitespace() && ch != '\n' {
            Class::Space
        } else {
            Class::Other
        }
    };

    let mut words = Vec::new();
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return words;
    };
    let mut start = 0;
    let mut prev = class(first);
    for (idx, ch) in chars {
        let current = class(ch);
        if current != prev || current == Class::Other {
            words.push(start..idx);
            start = idx;
        }
        prev = current;
    }
    words.push(start..text.len());
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed_words<'a>(before: &'a str, after: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
        let (removed, inserted) = compare_words(before, after);
        (
            removed.into_iter().map(|range| &before[range]).collect(),
            inserted.into_iter().map(|range| &after[range]).collect(),
        )
    }

    #[test]
    fn split_words_groups_words_and_spaces() {
        let text = "a_b1  (x)\n\tÄö";
        let words: Vec<_> = split_words(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(words, ["a_b1", "  ", "(", "x", ")", "\n", "\t", "Äö"]);
        assert!(split_words("").is_empty());
    }

    #[test]
    fn compare_words_finds_changed_words() {
        assert_eq!(
            changed_words(
                "flags: \"--timeout=300\"",
                "flags: \"--timeout=600 --wait\""
            ),
            (vec!["300"], vec!["600 --wait"])
        );
        assert_eq!(changed_words("let x = 1;", "let x = 1;"), (vec![], vec![]));
        assert_eq!(changed_words("", "new"), (vec![], vec!["new"]));
        assert_eq!(changed_words("old", ""), (vec!["old"], vec![]));
        // Byte ranges stay on character boundaries.
        assert_eq!(
            changed_words("größe = 1", "größe = 2"),
            (vec!["1"], vec!["2"])
        );
    }

    fn test_identity(a: &str, b: &str) {
        let mut old = Rope::from(a);
        let new = Rope::from(b);
        compare_ropes(&old, &new).apply(&mut old);
        assert_eq!(old, new);
    }

    quickcheck::quickcheck! {
        fn test_compare_ropes(a: String, b: String) -> bool {
            let mut old = Rope::from(a);
            let new = Rope::from(b);
            compare_ropes(&old, &new).apply(&mut old);
            old == new
        }
    }

    #[test]
    fn compare_words_keeps_added_lines_whole() {
        // The added line could also be matched as `;\nlet c = 3`.
        assert_eq!(
            changed_words("let b = 2;", "let b = 2;\nlet c = 3;"),
            (vec![], vec!["\nlet c = 3;"])
        );
        // ... or as ` c = 3;\nlet`.
        assert_eq!(
            changed_words("let b = 2;", "let c = 3;\nlet b = 2;"),
            (vec![], vec!["let c = 3;\n"])
        );
        assert_eq!(
            changed_words("let a = 1;\nlet b = 2;", "let b = 2;"),
            (vec!["let a = 1;\n"], vec![])
        );
        // A new last line that ends like the line before it.
        assert_eq!(changed_words("}", "}\nx{}"), (vec![], vec!["\nx{}"]));
        assert_eq!(changed_words("}\nx{}", "}"), (vec!["\nx{}"], vec![]));
    }

    /// The text outside `ranges`, checking that they are ordered, disjoint and
    /// on character boundaries.
    fn unchanged_text(text: &str, ranges: &[Range<usize>]) -> Option<String> {
        let mut unchanged = String::new();
        let mut pos = 0;
        for range in ranges {
            if range.start < pos || range.start >= range.end {
                return None;
            }
            unchanged.push_str(text.get(pos..range.start)?);
            text.get(range.clone())?;
            pos = range.end;
        }
        unchanged.push_str(text.get(pos..)?);
        Some(unchanged)
    }

    quickcheck::quickcheck! {
        fn compare_words_leaves_the_common_text(a: String, b: String) -> bool {
            let (removed, inserted) = compare_words(&a, &b);
            let unchanged_a = unchanged_text(&a, &removed);
            unchanged_a.is_some() && unchanged_a == unchanged_text(&b, &inserted)
        }
    }

    #[test]
    fn equal_files() {
        test_identity("foo", "foo");
    }

    #[test]
    fn trailing_newline() {
        test_identity("foo\n", "foo");
        test_identity("foo", "foo\n");
    }

    #[test]
    fn new_file() {
        test_identity("", "foo");
    }

    #[test]
    fn deleted_file() {
        test_identity("foo", "");
    }
}
