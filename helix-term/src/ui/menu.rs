use std::cmp::Reverse;

use crate::{
    alt,
    compositor::{Callback, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
};
use helix_core::{fuzzy::MATCHER, unicode::segmentation::UnicodeSegmentation};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Utf32Str,
};
use tui::{buffer::Buffer as Surface, text::Span, widgets::Table};

pub use tui::widgets::{Cell, Row};

use helix_view::{
    editor::SmartTabConfig,
    graphics::{Modifier, Rect},
    input::KeyEvent,
    keyboard::{KeyCode, KeyModifiers},
    theme::Style,
    Editor,
};
use tui::layout::Constraint;

pub trait Item: Sync + Send + 'static {
    /// Additional editor state that is used for label calculation.
    type Data: Sync + Send + 'static;

    fn format(&self, data: &Self::Data) -> Row<'_>;
}

pub type MenuCallback<T> = Box<dyn Fn(&mut Editor, Option<&T>, MenuEvent)>;

pub struct Menu<T: Item> {
    options: Vec<T>,
    editor_data: T::Data,

    cursor: Option<usize>,

    /// (index, score)
    matches: Vec<(u32, u32)>,

    widths: Vec<Constraint>,

    callback_fn: MenuCallback<T>,

    scroll: usize,
    size: (u16, u16),
    viewport: (u16, u16),
    recalculate: bool,
    auto_close: bool,
    /// The text typed to filter the options, when typing filters them (see
    /// [`Menu::filter_on_type`]).
    filter: Option<String>,
}

impl<T: Item> Menu<T> {
    const LEFT_PADDING: usize = 1;

    // TODO: it's like a slimmed down picker, share code? (picker = menu + prompt with different
    // rendering)
    pub fn new(
        options: Vec<T>,
        editor_data: <T as Item>::Data,
        callback_fn: impl Fn(&mut Editor, Option<&T>, MenuEvent) + 'static,
    ) -> Self {
        let matches = (0..options.len() as u32).map(|i| (i, 0)).collect();
        Self {
            options,
            editor_data,
            matches,
            cursor: None,
            widths: Vec::new(),
            callback_fn: Box::new(callback_fn),
            scroll: 0,
            size: (0, 0),
            viewport: (0, 0),
            recalculate: true,
            auto_close: false,
            filter: None,
        }
    }

    pub fn reset_cursor(&mut self) {
        self.cursor = None;
        self.scroll = 0;
        self.recalculate = true;
    }

    pub fn update_options(&mut self) -> (&mut Vec<(u32, u32)>, &mut Vec<T>) {
        self.recalculate = true;
        (&mut self.matches, &mut self.options)
    }

    pub fn ensure_cursor_in_bounds(&mut self) {
        if self.matches.is_empty() {
            self.cursor = None;
            self.scroll = 0;
        } else {
            self.scroll = 0;
            self.recalculate = true;
            if let Some(cursor) = &mut self.cursor {
                *cursor = (*cursor).min(self.matches.len() - 1)
            }
        }
    }

    pub fn clear(&mut self) {
        self.matches.clear();

        // reset cursor position
        self.cursor = None;
        self.scroll = 0;
    }

    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        let max_index = len.saturating_sub(1);
        let pos = self.cursor.map_or(max_index, |i| (i + max_index) % len) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_half_page_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        let max_index = len.saturating_sub((self.list_height() / 2).max(1));
        let pos = self.cursor.map_or(max_index, |i| (i + max_index) % len) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        let pos = self.cursor.map_or(0, |i| i + 1) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_half_page_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        let pos = self
            .cursor
            .map_or(0, |i| i + (self.list_height() / 2).max(1))
            % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn auto_close(mut self, auto_close: bool) -> Self {
        self.auto_close = auto_close;
        self
    }

    /// Lets typing filter the options, as in pickers: the best matches come
    /// first, and the typed text shows above them. Menus whose filter is typed
    /// elsewhere, such as completion in the document, leave this off.
    pub fn filter_on_type(mut self, filter_on_type: bool) -> Self {
        self.filter = filter_on_type.then(String::new);
        self
    }

    /// The rows above the options that show the filter.
    fn filter_rows(&self) -> u16 {
        u16::from(
            self.filter
                .as_ref()
                .is_some_and(|filter| !filter.is_empty()),
        )
    }

    /// The rows that show options.
    fn list_height(&self) -> usize {
        self.size.1.saturating_sub(self.filter_rows()) as usize
    }

    /// Matches the options against the filter, best first, and selects the
    /// best match. Options that match equally keep their order.
    fn apply_filter(&mut self) {
        let Some(filter) = &self.filter else {
            return;
        };

        let pattern = Pattern::parse(filter, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        let mut buf = Vec::new();

        self.matches.clear();
        for (index, option) in self.options.iter().enumerate() {
            let text = row_text(&option.format(&self.editor_data));
            if let Some(score) = pattern.score(Utf32Str::new(&text, &mut buf), &mut matcher) {
                self.matches.push((index as u32, score));
            }
        }
        self.matches
            .sort_by_key(|&(index, score)| (Reverse(score), index));

        self.cursor = (!self.matches.is_empty()).then_some(0);
        self.scroll = 0;
        self.recalculate = true;
    }

    /// Changes the filter, re-filters the options and reports the selection.
    fn edit_filter(&mut self, cx: &mut Context, edit: impl FnOnce(&mut String)) {
        let Some(filter) = &mut self.filter else {
            return;
        };
        edit(filter);
        self.apply_filter();
        (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
    }

    fn recalculate_size(&mut self, viewport: (u16, u16)) {
        let n = self
            .options
            .first()
            .map(|option| option.format(&self.editor_data).cells.len())
            .unwrap_or_default();
        let max_lens = self.options.iter().fold(vec![0; n], |mut acc, option| {
            let row = option.format(&self.editor_data);
            // maintain max for each column
            for (acc, cell) in acc.iter_mut().zip(row.cells.iter()) {
                let width = cell.content.width();
                if width > *acc {
                    *acc = width;
                }
            }

            acc
        });

        let filter_rows = self.filter_rows() as usize;
        let height = self
            .matches
            .len()
            .min(10)
            .min((viewport.1 as usize).saturating_sub(filter_rows));
        // do all the matches fit on a single screen?
        let fits = self.matches.len() <= height;

        let mut len = max_lens.iter().sum::<usize>() + n;

        if !fits {
            len += 1; // +1: reserve some space for scrollbar
        }

        len += Self::LEFT_PADDING;
        let width = len.min(viewport.0 as usize);

        self.widths = max_lens
            .into_iter()
            .map(|len| Constraint::Length(len as u16))
            .collect();

        self.size = (width as u16, (height + filter_rows) as u16);

        // adjust scroll offsets if size changed
        self.adjust_scroll();
        self.recalculate = false;
    }

    fn adjust_scroll(&mut self) {
        let win_height = self.list_height();
        if let Some(cursor) = self.cursor {
            let mut scroll = self.scroll;
            if cursor > (win_height + scroll).saturating_sub(1) {
                // scroll down
                scroll += cursor - (win_height + scroll).saturating_sub(1)
            } else if cursor < scroll {
                // scroll up
                scroll = cursor
            }
            self.scroll = scroll;
        }
    }

    pub fn selection(&self) -> Option<&T> {
        self.cursor.and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &self.options[*index as usize])
        })
    }

    pub fn selection_mut(&mut self) -> Option<&mut T> {
        self.cursor.and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &mut self.options[*index as usize])
        })
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Draws the typed filter, with the count of matching options on the right.
    fn render_filter_line(&self, area: Rect, surface: &mut Surface, style: Style) {
        let filter = self.filter.as_deref().unwrap_or_default();
        let count = format!("{}/{}", self.matches.len(), self.options.len());
        let area = area.clip_left(Self::LEFT_PADDING as u16).clip_right(1);
        let count_width = count.len() as u16;

        surface.set_stringn(
            area.x,
            area.y,
            filter,
            area.width.saturating_sub(count_width + 1) as usize,
            style,
        );
        if area.width > count_width {
            surface.set_stringn(
                area.right() - count_width,
                area.y,
                &count,
                count_width as usize,
                style,
            );
        }
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }
}

impl<T: Item + PartialEq> Menu<T> {
    pub fn replace_option(&mut self, old_option: &impl PartialEq<T>, new_option: T) {
        for option in &mut self.options {
            if old_option == option {
                *option = new_option;
                break;
            }
        }
    }
}

use super::PromptEvent as MenuEvent;

impl<T: Item + 'static> Component for Menu<T> {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event = match event {
            Event::Key(event) => *event,
            _ => return EventResult::Ignored(None),
        };

        let close_fn: Option<Callback> = Some(Box::new(|compositor: &mut Compositor, _| {
            // remove the layer
            compositor.pop();
        }));

        // Ignore tab key when supertab is turned on in order not to interfere
        // with it. (Is there a better way to do this?)
        if (event == key!(Tab) || event == shift!(Tab))
            && cx.editor.config().auto_completion
            && matches!(
                cx.editor.config().smart_tab,
                Some(SmartTabConfig {
                    enable: true,
                    supersede_menu: true,
                })
            )
        {
            return EventResult::Ignored(None);
        }

        match event {
            // esc or ctrl-c aborts the completion and closes the menu
            key!(Esc) | ctrl!('c') => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Consumed(close_fn);
            }
            // arrow up/ctrl-p/shift-tab prev completion choice (including updating the doc)
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                // arrow down/ctrl-n/tab advances completion choice (including updating the doc)
                self.move_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageUp) | ctrl!('u') => {
                // page up moves back in the completion choice (including updating the doc)
                self.move_half_page_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageDown) | ctrl!('d') => {
                // page down advances completion choice (including updating the doc)
                self.move_half_page_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Enter) => {
                if let Some(selection) = self.selection() {
                    (self.callback_fn)(cx.editor, Some(selection), MenuEvent::Validate);
                    return EventResult::Consumed(close_fn);
                } else if self.filter.is_some() {
                    // Nothing matches the filter: stay open so it can be fixed.
                    return EventResult::Consumed(None);
                } else {
                    return EventResult::Ignored(close_fn);
                }
            }
            key!(Backspace) | ctrl!('h') if self.filter.is_some() => {
                self.edit_filter(cx, |filter| {
                    filter.pop();
                });
                return EventResult::Consumed(None);
            }
            ctrl!('w') | alt!(Backspace) | ctrl!(Backspace) if self.filter.is_some() => {
                self.edit_filter(cx, delete_word_backwards);
                return EventResult::Consumed(None);
            }
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
            } if self.filter.is_some()
                && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.edit_filter(cx, |filter| filter.push(c));
                return EventResult::Consumed(None);
            }
            _ if self.auto_close => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Consumed(close_fn);
            }
            _ => (),
        }
        // for some events, we want to process them but send ignore, specifically all input except
        // tab/enter/ctrl-k or whatever will confirm the selection/ ctrl-n/ctrl-p for scroll.
        // EventResult::Consumed(None)
        EventResult::Ignored(None)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        if viewport != self.viewport || self.recalculate {
            self.recalculate_size(viewport);
        }

        Some(self.size)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let style = theme
            .try_get("ui.menu")
            .unwrap_or_else(|| theme.get("ui.text"));
        let selected = theme.get("ui.menu.selected");

        surface.clear_with(area, style);

        let filter_rows = self.filter_rows();
        if filter_rows != 0 {
            self.render_filter_line(area.with_height(1), surface, theme.get("ui.text"));
        }
        let area = area.clip_top(filter_rows);

        let scroll = self.scroll;

        let options: Vec<_> = self
            .matches
            .iter()
            .map(|(index, _score)| {
                // (index, self.options.get(*index).unwrap()) // get_unchecked
                &self.options[*index as usize] // get_unchecked
            })
            .collect();

        let len = options.len();

        let win_height = area.height as usize;

        let highlight_style = theme.get("special").add_modifier(Modifier::BOLD);
        let mut highlighter = self
            .filter
            .as_deref()
            .filter(|filter| !filter.is_empty())
            .map(|filter| {
                let pattern = Pattern::parse(filter, CaseMatching::Smart, Normalization::Smart);
                let mut matcher = MATCHER.lock();
                matcher.config = Config::DEFAULT;
                (pattern, matcher)
            });
        let (mut buf, mut indices) = (Vec::new(), Vec::new());

        let rows = options.iter().map(|option| {
            let row = option.format(&self.editor_data);
            let Some((pattern, matcher)) = &mut highlighter else {
                return row;
            };
            indices.clear();
            let text = row_text(&row);
            pattern.indices(Utf32Str::new(&text, &mut buf), matcher, &mut indices);
            indices.sort_unstable();
            indices.dedup();
            highlight_row(row, &indices, highlight_style)
        });
        let table = Table::new(rows)
            .style(style)
            .highlight_style(selected)
            .column_spacing(1)
            .widths(&self.widths);

        use tui::widgets::TableState;

        table.render_table(
            area.clip_left(Self::LEFT_PADDING as u16).clip_right(1),
            surface,
            &mut TableState {
                offset: scroll,
                selected: self.cursor,
            },
            false,
        );

        let render_borders = cx.editor.menu_border();

        if !render_borders {
            if let Some(cursor) = self.cursor {
                let offset_from_top = cursor - scroll;
                let left = &mut surface[(area.left(), area.y + offset_from_top as u16)];
                left.set_style(selected);
                let right = &mut surface[(
                    area.right().saturating_sub(1),
                    area.y + offset_from_top as u16,
                )];
                right.set_style(selected);
            }
        }

        let fits = len <= win_height;

        let scroll_style = theme.get("ui.menu.scroll");
        if !fits {
            let scroll_height = win_height.pow(2).div_ceil(len).min(win_height);
            let scroll_line = (win_height - scroll_height) * scroll
                / std::cmp::max(1, len.saturating_sub(win_height));

            let mut cell;
            for i in 0..win_height {
                cell = &mut surface[(area.right() - 1, area.top() + i as u16)];

                let half_block = if render_borders { "▌" } else { "▐" };

                if scroll_line <= i && i < scroll_line + scroll_height {
                    // Draw scroll thumb
                    cell.set_symbol(half_block);
                    cell.set_fg(scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset));
                } else if !render_borders {
                    // Draw scroll track
                    cell.set_symbol(half_block);
                    cell.set_fg(scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset));
                }
            }
        }
    }
}

/// The text that a filter matches in a row: the first line of each cell, with
/// a space between cells.
fn row_text(row: &Row) -> String {
    let mut text = String::new();
    for (index, cell) in row.cells.iter().enumerate() {
        if index != 0 {
            text.push(' ');
        }
        if let Some(line) = cell.content.lines.first() {
            for span in &line.0 {
                text.push_str(&span.content);
            }
        }
    }
    text
}

/// Patches `highlight` onto the graphemes of the row that a filter matched.
/// `indices` are sorted indices into [`row_text`]; nucleo matches the first
/// character of each grapheme, so they count graphemes.
fn highlight_row<'a>(mut row: Row<'a>, indices: &[u32], highlight: Style) -> Row<'a> {
    let mut indices = indices.iter().copied().peekable();
    let mut index = 0u32;

    for cell in &mut row.cells {
        if let Some(line) = cell.content.lines.first_mut() {
            let mut spans = Vec::with_capacity(line.0.len());
            for span in line.0.drain(..) {
                let mut run = String::new();
                let mut run_matched = false;
                for grapheme in span.content.graphemes(true) {
                    let matched = indices.next_if_eq(&index).is_some();
                    index += 1;
                    if matched != run_matched && !run.is_empty() {
                        let style = if run_matched {
                            span.style.patch(highlight)
                        } else {
                            span.style
                        };
                        spans.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    run_matched = matched;
                    run.push_str(grapheme);
                }
                if !run.is_empty() {
                    let style = if run_matched {
                        span.style.patch(highlight)
                    } else {
                        span.style
                    };
                    spans.push(Span::styled(run, style));
                }
            }
            line.0 = spans;
        }
        // The space between cells.
        index += 1;
    }

    row
}

/// Deletes the last word of the filter and the spaces after it.
fn delete_word_backwards(filter: &mut String) {
    let end = filter.trim_end().len();
    let start = filter[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    filter.truncate(start);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Action(&'static str);

    impl Item for Action {
        type Data = ();

        fn format(&self, _data: &Self::Data) -> Row<'_> {
            self.0.into()
        }
    }

    fn menu(options: &[&'static str]) -> Menu<Action> {
        let options = options.iter().map(|&option| Action(option)).collect();
        Menu::new(options, (), |_editor, _option, _event| {}).filter_on_type(true)
    }

    fn filter(menu: &mut Menu<Action>, filter: &str) -> Vec<&'static str> {
        menu.filter = Some(filter.to_string());
        menu.apply_filter();
        menu.matches
            .iter()
            .map(|&(index, _score)| menu.options[index as usize].0)
            .collect()
    }

    const ACTIONS: [&str; 4] = [
        "Extract to function",
        "Inline variable",
        "Extract to variable",
        "Organize imports",
    ];

    #[test]
    fn filter_keeps_matches_best_first() {
        let mut menu = menu(&ACTIONS);

        assert_eq!(filter(&mut menu, "orgimp"), ["Organize imports"]);
        // Equal matches keep their order.
        assert_eq!(
            filter(&mut menu, "variable"),
            ["Inline variable", "Extract to variable"]
        );
        // Atoms narrow together, as in pickers.
        assert_eq!(filter(&mut menu, "ext var"), ["Extract to variable"]);
        assert_eq!(
            menu.selection().map(|action| action.0),
            Some("Extract to variable")
        );
    }

    #[test]
    fn empty_filter_shows_all_options_in_order() {
        let mut menu = menu(&ACTIONS);

        assert_eq!(filter(&mut menu, ""), ACTIONS);
        assert_eq!(menu.cursor, Some(0));
    }

    #[test]
    fn no_match_leaves_nothing_selected() {
        let mut menu = menu(&ACTIONS);

        assert!(filter(&mut menu, "zzz").is_empty());
        assert!(menu.selection().is_none());
        // Moving through an empty menu does nothing.
        menu.move_down();
        menu.move_up();
        menu.move_half_page_down();
        menu.move_half_page_up();
        assert!(menu.selection().is_none());
    }

    #[test]
    fn filter_line_takes_a_row() {
        let mut menu = menu(&ACTIONS);

        assert_eq!(menu.required_size((80, 20)).unwrap().1, 4);
        filter(&mut menu, "ext");
        assert_eq!(menu.required_size((80, 20)).unwrap().1, 3);
        assert_eq!(menu.list_height(), 2);
        filter(&mut menu, "zzz");
        assert_eq!(menu.required_size((80, 20)).unwrap().1, 1);
        assert_eq!(menu.list_height(), 0);
    }

    #[test]
    fn highlight_spans_cells() {
        let highlight = Style::default().add_modifier(Modifier::BOLD);
        let row = Row::new(["ab", "cd"]);
        assert_eq!(row_text(&row), "ab cd");

        // "b" in the first cell and "c" in the second; index 2 is the space.
        let row = highlight_row(row, &[1, 3], highlight);
        let runs: Vec<Vec<_>> = row
            .cells
            .iter()
            .map(|cell| {
                cell.content.lines[0]
                    .0
                    .iter()
                    .map(|span| (span.content.to_string(), span.style == highlight))
                    .collect()
            })
            .collect();
        assert_eq!(
            runs,
            [
                vec![("a".to_string(), false), ("b".to_string(), true)],
                vec![("c".to_string(), true), ("d".to_string(), false)],
            ]
        );
    }

    #[test]
    fn delete_word_removes_the_last_word() {
        let mut filter = String::from("extract to ");
        delete_word_backwards(&mut filter);
        assert_eq!(filter, "extract ");
        delete_word_backwards(&mut filter);
        assert_eq!(filter, "");
        delete_word_backwards(&mut filter);
        assert_eq!(filter, "");

        let mut filter = String::from("ext\u{3000}fün");
        delete_word_backwards(&mut filter);
        assert_eq!(filter, "ext\u{3000}");
    }
}
