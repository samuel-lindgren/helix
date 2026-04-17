use std::fmt::Write;

use helix_core::syntax::config::LanguageServerFeature;

use crate::{
    editor::GutterType,
    graphics::{Color, Style, UnderlineStyle},
    Document, Editor, Theme, View,
};

fn count_digits(n: usize) -> usize {
    (usize::checked_ilog10(n).unwrap_or(0) + 1) as usize
}

pub type GutterFn<'doc> = Box<dyn FnMut(usize, bool, bool, &mut String) -> Option<Style> + 'doc>;
pub type Gutter =
    for<'doc> fn(&'doc Editor, &'doc Document, &View, &Theme, bool, usize) -> GutterFn<'doc>;

impl GutterType {
    pub fn style<'doc>(
        self,
        editor: &'doc Editor,
        doc: &'doc Document,
        view: &View,
        theme: &Theme,
        is_focused: bool,
    ) -> GutterFn<'doc> {
        match self {
            GutterType::Diagnostics => {
                diagnostics_or_breakpoints(editor, doc, view, theme, is_focused)
            }
            GutterType::LineNumbers => line_numbers(editor, doc, view, theme, is_focused),
            GutterType::Spacer => padding(editor, doc, view, theme, is_focused),
            GutterType::Diff => diff(editor, doc, view, theme, is_focused),
        }
    }

    pub fn width(self, view: &View, doc: &Document) -> usize {
        match self {
            GutterType::Diagnostics => 1,
            GutterType::LineNumbers => line_numbers_width(view, doc),
            GutterType::Spacer => 1,
            GutterType::Diff => 1,
        }
    }
}

pub fn diagnostic<'doc>(
    _editor: &'doc Editor,
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    let warning = theme.get("warning");
    let error = theme.get("error");
    let info = theme.get("info");
    let hint = theme.get("hint");
    let diagnostics = &doc.diagnostics;

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line {
                return None;
            }
            use helix_core::diagnostic::Severity;
            let first_diag_idx_maybe_on_line = diagnostics.partition_point(|d| d.line < line);
            let diagnostics_on_line = diagnostics[first_diag_idx_maybe_on_line..]
                .iter()
                .take_while(|d| {
                    d.line == line
                        && d.provider.language_server_id().is_none_or(|id| {
                            doc.language_servers_with_feature(LanguageServerFeature::Diagnostics)
                                .any(|ls| ls.id() == id)
                        })
                });
            diagnostics_on_line.max_by_key(|d| d.severity).map(|d| {
                write!(out, "●").ok();
                match d.severity {
                    Some(Severity::Error) => error,
                    Some(Severity::Warning) | None => warning,
                    Some(Severity::Info) => info,
                    Some(Severity::Hint) => hint,
                }
            })
        },
    )
}

pub fn diff<'doc>(
    _editor: &'doc Editor,
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    // Working-tree diff (vs HEAD) styles — the existing behavior.
    let head_added = theme.get("diff.plus.gutter");
    let head_deleted = theme.get("diff.minus.gutter");
    let head_modified = theme.get("diff.delta.gutter");

    // Branch-diff overlay styles. Use `try_get_exact` (not `get`) so we don't
    // fall back to the parent `diff.plus.gutter` scope, which would make the
    // overlay visually indistinguishable from HEAD changes. If the theme
    // doesn't define the branch scopes, use a hardcoded distinct color so
    // the overlay stays visible.
    let branch_added = theme
        .try_get_exact("diff.plus.gutter.branch")
        .unwrap_or_else(|| Style::default().fg(Color::Cyan));
    let branch_deleted = theme
        .try_get_exact("diff.minus.gutter.branch")
        .unwrap_or_else(|| Style::default().fg(Color::LightMagenta));
    let branch_modified = theme
        .try_get_exact("diff.delta.gutter.branch")
        .unwrap_or_else(|| Style::default().fg(Color::Cyan));

    let head_diff = doc.diff_handle().map(|h| h.load());
    let branch_diff = doc.branch_diff_handle().map(|h| h.load());

    if head_diff.is_none() && branch_diff.is_none() {
        return Box::new(|_, _, _, _| None);
    }

    // Two independent monotonic cursors — one per diff source. At each visual
    // line we advance both past anything ending before `line`, then check HEAD
    // first (priority rule: working-tree edits win over committed-in-branch
    // markers on any overlapping line).
    let mut head_i: u32 = 0;
    let mut head_hunk = head_diff.as_ref().map(|h| h.nth_hunk(0));
    let mut branch_i: u32 = 0;
    let mut branch_hunk = branch_diff.as_ref().map(|h| h.nth_hunk(0));

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            let line_u = line as u32;

            // Advance + check HEAD cursor.
            if let (Some(hunks), Some(hunk)) = (head_diff.as_ref(), head_hunk.as_mut()) {
                // truncating the line is fine here because we don't compute diffs
                // for files with more lines than i32::MAX anyways
                // we need to special case removals here
                // these technically do not have a range of lines to highlight (`hunk.after.start == hunk.after.end`).
                // However we still want to display these hunks correctly we must not yet skip to the next hunk here
                while hunk.after.end < line_u || !hunk.is_pure_removal() && line_u == hunk.after.end
                {
                    head_i += 1;
                    *hunk = hunks.nth_hunk(head_i);
                }

                if hunk.after.start <= line_u {
                    let (icon, style) = if hunk.is_pure_insertion() {
                        ("▍", head_added)
                    } else if hunk.is_pure_removal() {
                        if !first_visual_line {
                            return None;
                        }
                        ("▔", head_deleted)
                    } else {
                        ("▍", head_modified)
                    };

                    write!(out, "{}", icon).unwrap();
                    return Some(style);
                }
            }

            // HEAD didn't claim this line — fall through to the branch overlay.
            if let (Some(hunks), Some(hunk)) = (branch_diff.as_ref(), branch_hunk.as_mut()) {
                while hunk.after.end < line_u || !hunk.is_pure_removal() && line_u == hunk.after.end
                {
                    branch_i += 1;
                    *hunk = hunks.nth_hunk(branch_i);
                }

                if hunk.after.start <= line_u {
                    let (icon, style) = if hunk.is_pure_insertion() {
                        ("▍", branch_added)
                    } else if hunk.is_pure_removal() {
                        if !first_visual_line {
                            return None;
                        }
                        ("▔", branch_deleted)
                    } else {
                        ("▍", branch_modified)
                    };

                    write!(out, "{}", icon).unwrap();
                    return Some(style);
                }
            }

            None
        },
    )
}

pub fn line_numbers<'doc>(
    editor: &'doc Editor,
    doc: &'doc Document,
    view: &View,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let text = doc.text().slice(..);
    let width = line_numbers_width(view, doc);

    let last_line_in_view = view.estimate_last_doc_line(doc);

    // Whether to draw the line number for the last line of the
    // document or not.  We only draw it if it's not an empty line.
    let draw_last = text.line_to_byte(last_line_in_view) < text.len_bytes();

    let linenr = theme.get("ui.linenr");
    let linenr_select = theme.get("ui.linenr.selected");

    let current_line = doc
        .text()
        .char_to_line(doc.selection(view.id).primary().cursor(text));

    let line_number = editor.config().line_number;
    let mode = editor.mode;

    Box::new(
        move |line: usize, selected: bool, first_visual_line: bool, out: &mut String| {
            if line == last_line_in_view && !draw_last {
                write!(out, "{:>1$}", '~', width).unwrap();
                Some(linenr)
            } else {
                use crate::{document::Mode, editor::LineNumber};

                let relative = line_number == LineNumber::Relative
                    && mode != Mode::Insert
                    && is_focused
                    && current_line != line;

                let display_num = if relative {
                    current_line.abs_diff(line)
                } else {
                    line + 1
                };

                let style = if selected && is_focused {
                    linenr_select
                } else {
                    linenr
                };

                if first_visual_line {
                    write!(out, "{:>1$}", display_num, width).unwrap();
                } else {
                    write!(out, "{:>1$}", " ", width).unwrap();
                }

                first_visual_line.then_some(style)
            }
        },
    )
}

/// The width of a "line-numbers" gutter
///
/// The width of the gutter depends on the number of lines in the document,
/// whether there is content on the last line (the `~` line), and the
/// `editor.gutters.line-numbers.min-width` settings.
fn line_numbers_width(view: &View, doc: &Document) -> usize {
    let text = doc.text();
    let last_line = text.len_lines().saturating_sub(1);
    let draw_last = text.line_to_byte(last_line) < text.len_bytes();
    let last_drawn = if draw_last { last_line + 1 } else { last_line };
    let digits = count_digits(last_drawn);
    let n_min = view.gutters.line_numbers.min_width;
    digits.max(n_min)
}

pub fn padding<'doc>(
    _editor: &'doc Editor,
    _doc: &'doc Document,
    _view: &View,
    _theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    Box::new(|_line: usize, _selected: bool, _first_visual_line: bool, _out: &mut String| None)
}

pub fn breakpoints<'doc>(
    editor: &'doc Editor,
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    let error = theme.get("error");
    let info = theme.get("info");
    let breakpoint_style = theme.get("ui.debug.breakpoint");

    let breakpoints = doc.path().and_then(|path| editor.breakpoints.get(path));

    let breakpoints = match breakpoints {
        Some(breakpoints) => breakpoints,
        None => return Box::new(move |_, _, _, _| None),
    };

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line {
                return None;
            }
            let breakpoint = breakpoints
                .iter()
                .find(|breakpoint| breakpoint.line == line)?;

            let style = if breakpoint.condition.is_some() && breakpoint.log_message.is_some() {
                error.underline_style(UnderlineStyle::Line)
            } else if breakpoint.condition.is_some() {
                error
            } else if breakpoint.log_message.is_some() {
                info
            } else {
                breakpoint_style
            };

            let sym = if breakpoint.verified { "●" } else { "◯" };
            write!(out, "{}", sym).unwrap();
            Some(style)
        },
    )
}

fn execution_pause_indicator<'doc>(
    editor: &'doc Editor,
    doc: &'doc Document,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let style = theme.get("ui.debug.active");
    let current_stack_frame = editor.current_stack_frame();
    let frame_line = current_stack_frame.map(|frame| frame.line.saturating_sub(1));
    let frame_source_path = current_stack_frame.map(|frame| {
        frame
            .source
            .as_ref()
            .and_then(|source| source.path.as_ref())
    });
    let should_display_for_current_doc =
        doc.path().is_some() && frame_source_path.unwrap_or(None) == doc.path();

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line
                || !is_focused
                || line != frame_line?
                || !should_display_for_current_doc
            {
                return None;
            }

            let sym = "▶";
            write!(out, "{}", sym).unwrap();
            Some(style)
        },
    )
}

pub fn diagnostics_or_breakpoints<'doc>(
    editor: &'doc Editor,
    doc: &'doc Document,
    view: &View,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let mut diagnostics = diagnostic(editor, doc, view, theme, is_focused);
    let mut breakpoints = breakpoints(editor, doc, view, theme, is_focused);
    let mut execution_pause_indicator = execution_pause_indicator(editor, doc, theme, is_focused);

    Box::new(move |line, selected, first_visual_line: bool, out| {
        execution_pause_indicator(line, selected, first_visual_line, out)
            .or_else(|| breakpoints(line, selected, first_visual_line, out))
            .or_else(|| diagnostics(line, selected, first_visual_line, out))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::document::Document;
    use crate::editor::{Config, GutterConfig, GutterLineNumbersConfig};
    use crate::graphics::Rect;
    use crate::DocumentId;
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope};

    #[test]
    fn test_default_gutter_widths() {
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 5);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[2].width(&view, &doc), 3);
        assert_eq!(view.gutters.layout[3].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[4].width(&view, &doc), 1);
    }

    #[test]
    fn test_configured_gutter_widths() {
        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics],
            ..Default::default()
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 1);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);

        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics, GutterType::LineNumbers],
            line_numbers: GutterLineNumbersConfig { min_width: 10 },
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 2);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc), 10);
    }

    #[test]
    fn test_line_numbers_gutter_width_resizes() {
        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics, GutterType::LineNumbers],
            line_numbers: GutterLineNumbersConfig { min_width: 1 },
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("a\nb");
        let doc_short = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        let rope = Rope::from_str("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np");
        let doc_long = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 2);
        assert_eq!(view.gutters.layout[1].width(&view, &doc_short), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc_long), 2);
    }
}
