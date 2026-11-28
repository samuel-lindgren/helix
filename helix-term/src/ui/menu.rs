use std::cmp::Reverse;

use crate::{
    alt,
    compositor::{Callback, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
};
use helix_core::{
    fuzzy::MATCHER,
    unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr},
};
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
    /// What was typed to filter the options, when typing filters them (see
    /// [`Menu::filter_on_type`]).
    filter: Option<Filter>,
}

/// The text typed to filter a menu.
#[derive(Default)]
struct Filter {
    text: String,
    /// `text` parsed as the query of a picker.
    pattern: Pattern,
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
        self.filter = filter_on_type.then(Filter::default);
        self
    }

    /// The rows above the options that show the filter.
    fn filter_rows(&self) -> u16 {
        u16::from(
            self.filter
                .as_ref()
                .is_some_and(|filter| !filter.text.is_empty()),
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

        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        let mut haystack = Haystack::default();

        self.matches.clear();
        for (index, option) in self.options.iter().enumerate() {
            let text = row_text(&option.format(&self.editor_data));
            if let Some(score) = filter.pattern.score(haystack.utf32(&text), &mut matcher) {
                self.matches.push((index as u32, score));
            }
        }
        self.matches
            .sort_by_key(|&(index, score)| (Reverse(score), index));

        self.cursor = (!self.matches.is_empty()).then_some(0);
        self.scroll = 0;
        self.recalculate = true;
    }

    /// Changes the filter text, and reports the new selection when the query
    /// changed with it.
    fn edit_filter(&mut self, cx: &mut Context, edit: impl FnOnce(&mut String)) {
        if self.edit_filter_text(edit) {
            (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
        }
    }

    /// Changes the filter text. When that changes the query, the options are
    /// filtered again and the best match is selected; edits that leave the
    /// query as it was, such as a space at the end, keep the selection.
    /// Returns whether the query changed.
    fn edit_filter_text(&mut self, edit: impl FnOnce(&mut String)) -> bool {
        let Some(filter) = &mut self.filter else {
            return false;
        };
        let old_text = filter.text.clone();
        edit(&mut filter.text);
        if filter.text == old_text {
            return false;
        }
        // The filter line may have appeared, gone or grown.
        self.recalculate = true;

        let pattern = Pattern::parse(&filter.text, CaseMatching::Smart, Normalization::Smart);
        if pattern.atoms == filter.pattern.atoms {
            return false;
        }
        filter.pattern = pattern;
        self.apply_filter();
        true
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
        // A menu that filters keeps the height of all its options, so that it
        // stays in place while matches come and go.
        let rows = if self.filter.is_some() {
            self.options.len()
        } else {
            self.matches.len()
        };
        let height = rows
            .min(10)
            .min((viewport.1 as usize).saturating_sub(filter_rows));
        // do all the matches fit on a single screen?
        let fits = self.matches.len() <= height;

        let mut len = max_lens.iter().sum::<usize>() + n;

        if !fits {
            len += 1; // +1: reserve some space for scrollbar
        }

        len += Self::LEFT_PADDING;
        if let Some(filter) = self
            .filter
            .as_ref()
            .filter(|filter| !filter.text.is_empty())
        {
            // The filter line: the text, a space and the widest count.
            let count = format!("{0}/{0}", self.options.len());
            len = len.max(Self::LEFT_PADDING + filter.text.width() + 1 + count.len() + 1);
        }
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
        let Some(filter) = &self.filter else {
            return;
        };
        let count = format!("{}/{}", self.matches.len(), self.options.len());
        let area = area.clip_left(Self::LEFT_PADDING as u16).clip_right(1);
        let count_width = count.len() as u16;
        // The count gives way to the text when the line is narrow.
        let show_count = area.width > count_width + 1;
        let text_width = if show_count {
            area.width - count_width - 1
        } else {
            area.width
        };

        // What was typed last stays in view.
        surface.set_string_truncated(
            area.x,
            area.y,
            &filter.text,
            text_width as usize,
            |_| style,
            true,
            true,
        );
        if show_count {
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
            key!(Backspace) | shift!(Backspace) | ctrl!('h') if self.filter.is_some() => {
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
            } if self.filter.is_some() && is_text(modifiers) => {
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
        if filter_rows != 0 && area.height != 0 {
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
            .as_ref()
            .filter(|filter| !filter.pattern.atoms.is_empty())
            .map(|filter| {
                let mut matcher = MATCHER.lock();
                matcher.config = Config::DEFAULT;
                (&filter.pattern, matcher)
            });
        let (mut haystack, mut indices) = (Haystack::default(), Vec::new());

        let rows = options.iter().map(|option| {
            let row = option.format(&self.editor_data);
            let Some((pattern, matcher)) = &mut highlighter else {
                return row;
            };
            indices.clear();
            let text = row_text(&row);
            pattern.indices(haystack.utf32(&text), matcher, &mut indices);
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
            // The list can be empty below the filter line in a tiny area.
            if let Some(cursor) = self
                .cursor
                .filter(|&cursor| cursor >= scroll && cursor - scroll < win_height)
            {
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

/// Whether a character typed with `modifiers` is text for the filter. AltGr
/// arrives as Ctrl and Alt together from some terminals.
fn is_text(modifiers: KeyModifiers) -> bool {
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    !modifiers.intersects(ctrl_alt) || modifiers.contains(ctrl_alt)
}

/// Hands text to nucleo with one character per grapheme, so that match
/// indices count graphemes. `Utf32Str::new` hands on the bytes of text whose
/// graphemes all start with an ASCII character, such as an `e` with a combining
/// accent, and its indices then count bytes.
#[derive(Default)]
struct Haystack {
    chars: Vec<char>,
    ascii: Vec<u8>,
}

impl Haystack {
    fn utf32<'a>(&'a mut self, text: &'a str) -> Utf32Str<'a> {
        if text.is_ascii() && !text.contains("\r\n") {
            return Utf32Str::Ascii(text.as_bytes());
        }
        self.chars.clear();
        self.chars.extend(
            text.graphemes(true)
                .filter_map(|grapheme| grapheme.chars().next()),
        );
        if self.chars.iter().all(char::is_ascii) {
            // Nucleo matches ASCII text more fully than the same characters
            // as Unicode, so keep it ASCII.
            self.ascii.clear();
            self.ascii.extend(self.chars.iter().map(|&c| c as u8));
            Utf32Str::Ascii(&self.ascii)
        } else {
            Utf32Str::Unicode(&self.chars)
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
                    // Skip indices that fell between cells.
                    while indices.next_if(|&matched| matched < index).is_some() {}
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
        let text = filter.to_string();
        menu.edit_filter_text(|filter| *filter = text);
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

        filter(&mut menu, "ext");
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
    fn filter_line_takes_a_row_and_the_list_keeps_its_height() {
        let mut menu = menu(&ACTIONS);

        assert_eq!(menu.required_size((80, 20)).unwrap().1, 4);
        filter(&mut menu, "ext");
        assert_eq!(menu.required_size((80, 20)).unwrap().1, 5);
        assert_eq!(menu.list_height(), 4);
        filter(&mut menu, "zzz");
        assert_eq!(menu.required_size((80, 20)).unwrap().1, 5);
    }

    #[test]
    fn better_matches_move_up() {
        let mut menu = menu(&["Convert to raw string", "var"]);

        assert_eq!(filter(&mut menu, "var"), ["var", "Convert to raw string"]);
    }

    #[test]
    fn edits_that_keep_the_query_keep_the_selection() {
        let mut menu = menu(&ACTIONS);
        menu.move_down();
        menu.move_down();
        assert_eq!(
            menu.selection().map(|action| action.0),
            Some("Inline variable")
        );

        // Deleting from an empty filter changes nothing.
        assert!(!menu.edit_filter_text(|filter| {
            filter.pop();
        }));
        assert!(!menu.edit_filter_text(delete_word_backwards));
        assert_eq!(
            menu.selection().map(|action| action.0),
            Some("Inline variable")
        );

        filter(&mut menu, "a");
        menu.move_down();
        let selected = menu.selection().map(|action| action.0);
        // A space at the end only separates terms.
        assert!(!menu.edit_filter_text(|filter| filter.push(' ')));
        assert_eq!(menu.selection().map(|action| action.0), selected);
        assert_eq!(menu.filter_rows(), 1);
    }

    #[test]
    fn filter_line_keeps_the_end_of_a_long_filter() {
        let mut menu = menu(&["Yes", "No"]);
        filter(&mut menu, "abcdefghij");
        // The menu grows for the filter line as far as it can.
        // One column of padding on each side, the text, a space and "2/2".
        assert_eq!(menu.required_size((80, 20)).unwrap().0, 16);
        assert_eq!(menu.required_size((10, 20)).unwrap().0, 10);

        let area = Rect::new(0, 0, 10, 1);
        let mut surface = Surface::empty(area);
        menu.render_filter_line(area, &mut surface, Style::default());
        let line: String = (0..10).map(|x| surface[(x, 0)].symbol.clone()).collect();
        assert_eq!(line, " …ij  0/2 ");
    }

    #[test]
    fn highlight_steps_over_the_space_between_cells() {
        let highlight = Style::default().add_modifier(Modifier::BOLD);
        // "b", the space between the cells, and "c".
        let row = highlight_row(Row::new(["ab", "cd"]), &[1, 2, 3], highlight);
        let matched: Vec<_> = row
            .cells
            .iter()
            .flat_map(|cell| cell.content.lines[0].0.iter())
            .filter(|span| span.style == highlight)
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(matched, ["b", "c"]);
    }

    #[test]
    fn highlights_count_graphemes_of_decomposed_text() {
        // An `e` with a combining accent, which `Utf32Str::new` passes as bytes.
        let text = "cafe\u{301} au lait";
        let pattern = Pattern::parse("'lait", CaseMatching::Smart, Normalization::Smart);
        let mut matcher = nucleo::Matcher::default();
        let mut indices = Vec::new();
        pattern.indices(Haystack::default().utf32(text), &mut matcher, &mut indices);
        assert_eq!(indices, [8, 9, 10, 11]);
    }

    #[test]
    fn altgr_characters_are_text() {
        assert!(is_text(KeyModifiers::NONE));
        assert!(is_text(KeyModifiers::SHIFT));
        assert!(is_text(KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert!(!is_text(KeyModifiers::CONTROL));
        assert!(!is_text(KeyModifiers::ALT));
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

    #[cfg(feature = "integration")]
    fn editor() -> Editor {
        use std::sync::Arc;

        use crate::config::Config;

        let config = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));
        let handlers = crate::handlers::setup(config.clone());
        Editor::new(
            Rect::new(0, 0, 120, 40),
            Arc::new(helix_view::theme::Loader::new(&[])),
            Arc::new(arc_swap::ArcSwap::from_pointee(
                helix_core::syntax::Loader::default(),
            )),
            Arc::new(arc_swap::access::Map::new(config, |config: &Config| {
                &config.editor
            })),
            handlers,
        )
    }

    #[cfg(feature = "integration")]
    #[tokio::test(flavor = "multi_thread")]
    async fn select_answers_while_nothing_matches() {
        use std::{cell::RefCell, rc::Rc};

        let mut editor = editor();
        let mut jobs = crate::job::Jobs::new();
        let mut cx = Context {
            editor: &mut editor,
            scroll: None,
            jobs: &mut jobs,
        };
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let mut select = crate::ui::Select::new(
            "Restart the server?",
            [Action("Yes"), Action("No")],
            (),
            move |_editor, action: Option<&Action>, event| {
                let event = match event {
                    MenuEvent::Update => "update",
                    MenuEvent::Validate => "validate",
                    MenuEvent::Abort => "abort",
                };
                recorded
                    .borrow_mut()
                    .push((action.map(|action| action.0), event));
            },
        );
        let mut press = |select: &mut crate::ui::Select<Action>, key| {
            select.handle_event(&Event::Key(key), &mut cx)
        };
        let alt_gr = |c| KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        };

        // `q` matches no option, so none is selected.
        assert!(matches!(
            press(&mut select, key!('q')),
            EventResult::Consumed(None)
        ));
        assert!(matches!(
            press(&mut select, key!(Down)),
            EventResult::Consumed(None)
        ));
        // Enter waits for a match.
        assert!(matches!(
            press(&mut select, key!(Enter)),
            EventResult::Consumed(None)
        ));
        // Shift-Backspace deletes, and AltGr types.
        press(&mut select, shift!(Backspace));
        press(&mut select, alt_gr('n'));
        press(&mut select, key!('q'));
        // Esc still answers, with nothing selected.
        assert!(matches!(
            press(&mut select, key!(Esc)),
            EventResult::Consumed(Some(_))
        ));

        assert_eq!(
            *events.borrow(),
            [
                (None, "update"),
                (None, "update"),
                (Some("Yes"), "update"),
                (Some("No"), "update"),
                (None, "update"),
                (None, "abort"),
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
