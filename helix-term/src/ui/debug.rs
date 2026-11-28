use std::borrow::Cow;

use crate::{
    compositor::{Callback, Component, Compositor, Context, Event, EventResult},
    ctrl,
    dap_display::{decode_byte_collection, load_byte_collection},
    key, shift,
    ui::{self, picker::query::PickerQuery, Prompt, PromptEvent},
};
use helix_core::{fuzzy::MATCHER, unicode::segmentation::UnicodeSegmentation, Position};
use helix_dap::{self as dap, registry::DebugAdapterId};
use helix_lsp::block_on;
use helix_view::{
    graphics::{CursorKind, Margin, Modifier, Rect},
    theme::Style,
    Editor,
};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans, Text as TuiText},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Widget},
};

const TITLE: &str = " Debug Variables ";
const HELP: &str = "up/down move | right expand | left collapse | / filter | esc close";
const EMPTY: &str = "No variables available for the current frame.";
const NO_MATCHES: &str = "No variables match the filter.";
/// Shown next to an empty filter line.
const FILTER_HINT: &str = "name, %type, %value";
/// The narrowest filter line: `/`, a column of query and the two columns that
/// the prompt keeps free at its end.
const MIN_FILTER_WIDTH: u16 = 4;
/// The query keeps at least this much of the filter line when the line also
/// shows the hint or the match count.
const MIN_QUERY_WIDTH: u16 = 12;
/// The columns a filter can name with `%`; text without a column matches the first one.
const FILTER_COLUMNS: [&str; 3] = ["name", "type", "value"];

pub struct DebugVariables {
    debugger_id: DebugAdapterId,
    roots: Vec<Node>,
    visible_paths: Vec<Vec<usize>>,
    cursor: usize,
    scroll: usize,
    page_size: usize,
    viewport: (u16, u16),
    status: Option<String>,
    /// The frame ID used to fetch the current scopes.
    /// When the active frame changes, the browser refreshes automatically.
    frame_id: Option<usize>,
    filter: Option<Filter>,
}

/// Narrows the rows to the variables that match a query typed after `/`.
///
/// A row stays when its variable matches, when one of its loaded children
/// matches at any depth, or when a row above it matches and no `!` term of the
/// query excludes it. Scopes do not match; their rows stay above matches.
/// Nothing is fetched from the debugger for the filter, and rows keep their
/// expanded state.
struct Filter {
    prompt: Prompt,
    query: PickerQuery,
    /// One pattern per entry of [`FILTER_COLUMNS`], `None` where the query has no text.
    patterns: [Option<Pattern>; 3],
    /// The `!` terms of each pattern, which also hide rows below a match.
    exclusions: [Option<Pattern>; 3],
    /// Whether keys go to the filter line rather than to the tree.
    editing: bool,
    /// Loaded variables that match.
    matched: usize,
    /// Loaded variables.
    total: usize,
    /// The node the cursor was last on, to go back to when a query that
    /// matches nothing is changed or cleared.
    anchor: Option<Vec<usize>>,
}

/// Where the rows of a filtered tree put the cursor after the query changes.
#[derive(Default)]
struct FilterTargets {
    /// The best visible match, and its row.
    best: Option<(Rank, usize)>,
    /// The first collapsed row that holds a match.
    holder: Option<usize>,
}

/// How well a node matches a filter.
#[derive(Clone, Copy)]
struct Rank {
    score: u32,
    /// The length of the text that the query was matched in. As in pickers,
    /// the shorter text wins a tie.
    len: usize,
}

#[derive(Debug, Clone)]
struct Node {
    kind: NodeKind,
    children: Children,
    expanded: bool,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Scope {
        name: String,
        presentation_hint: Option<String>,
        expensive: bool,
        variables_reference: usize,
    },
    Variable {
        name: String,
        value: String,
        ty: Option<String>,
        variables_reference: usize,
        indexed_variables: Option<usize>,
    },
}

#[derive(Debug, Clone)]
enum Children {
    Unloaded,
    Loaded(Vec<Node>),
    Failed(String),
}

struct Styles {
    base: Style,
    selected: Style,
    help: Style,
    scroll: Style,
    scope: Style,
    name: Style,
    ty: Style,
    value: Style,
    /// Patched onto the characters that a filter matched.
    matched: Style,
}

impl DebugVariables {
    pub const ID: &'static str = "dap-variables";

    pub fn new(
        debugger_id: DebugAdapterId,
        mut scopes: Vec<dap::Scope>,
        frame_id: Option<usize>,
    ) -> Self {
        scopes.sort_by_cached_key(Self::scope_sort_key);

        let roots = scopes.into_iter().map(Node::from_scope).collect();
        let mut variables = Self {
            debugger_id,
            roots,
            visible_paths: Vec::new(),
            cursor: 0,
            scroll: 0,
            page_size: 1,
            viewport: (0, 0),
            status: None,
            frame_id,
            filter: None,
        };
        variables.refresh_visible_paths();
        variables
    }

    /// Evaluate watch expressions and prepend a "Watch" scope to the tree.
    pub fn build_watch_scope(&mut self, editor: &Editor) {
        if editor.watch_expressions.is_empty() {
            return;
        }

        let frame_id = self.frame_id;
        let Some(debugger) = editor.debug_adapters.get_client(self.debugger_id) else {
            return;
        };

        let mut children = Vec::new();
        for expr in &editor.watch_expressions {
            let node = match block_on(debugger.eval(expr.clone(), frame_id)) {
                Ok(resp) => {
                    let (value, children_state) = if resp.variables_reference == 0 {
                        (sanitize_inline(resp.result), Children::Loaded(Vec::new()))
                    } else {
                        match load_byte_collection(
                            debugger,
                            resp.variables_reference,
                            resp.indexed_variables,
                        ) {
                            Some(decoded) => (
                                sanitize_inline(decoded.inline.clone()),
                                Children::Loaded(vec![Node::from_decoded_bytes(decoded)]),
                            ),
                            None => match block_on(debugger.variables(resp.variables_reference)) {
                                Ok(variables) => match decode_byte_collection(&variables) {
                                    Some(decoded) => (
                                        sanitize_inline(decoded.inline.clone()),
                                        Children::Loaded(vec![Node::from_decoded_bytes(decoded)]),
                                    ),
                                    None => (sanitize_inline(resp.result), Children::Unloaded),
                                },
                                Err(_) => (sanitize_inline(resp.result), Children::Unloaded),
                            },
                        }
                    };

                    Node {
                        kind: NodeKind::Variable {
                            name: expr.clone(),
                            value,
                            ty: resp.ty.map(sanitize_inline),
                            variables_reference: resp.variables_reference,
                            indexed_variables: resp.indexed_variables,
                        },
                        children: children_state,
                        expanded: false,
                    }
                }
                Err(e) => Node {
                    kind: NodeKind::Variable {
                        name: expr.clone(),
                        value: format!("<{}>", e),
                        ty: None,
                        variables_reference: 0,
                        indexed_variables: None,
                    },
                    children: Children::Loaded(Vec::new()),
                    expanded: false,
                },
            };
            children.push(node);
        }

        let watch_scope = Node {
            kind: NodeKind::Scope {
                name: "Watch".to_string(),
                presentation_hint: Some("watch".to_string()),
                expensive: false,
                variables_reference: 0,
            },
            children: Children::Loaded(children),
            expanded: true,
        };

        // Insert watch scope at the beginning.
        self.roots.insert(0, watch_scope);
    }

    pub fn expand_initial(&mut self, editor: &mut Editor) {
        let initial = self
            .roots
            .iter()
            .position(Node::is_local_scope)
            .or_else(|| (!self.roots.is_empty()).then_some(0));

        if let Some(index) = initial {
            let path = vec![index];
            let _ = self.expand_path(&path, editor);
            self.refresh_visible_paths();
        }
    }

    fn scope_sort_key(scope: &dap::Scope) -> (usize, bool, String) {
        let group =
            if Self::scope_matches(scope.presentation_hint.as_deref(), &scope.name, "locals") {
                0
            } else if Self::scope_matches(
                scope.presentation_hint.as_deref(),
                &scope.name,
                "arguments",
            ) {
                1
            } else if Self::scope_matches(
                scope.presentation_hint.as_deref(),
                &scope.name,
                "returnValue",
            ) {
                2
            } else if Self::scope_matches(
                scope.presentation_hint.as_deref(),
                &scope.name,
                "registers",
            ) {
                3
            } else {
                4
            };

        (group, scope.expensive, scope.name.to_ascii_lowercase())
    }

    fn scope_matches(hint: Option<&str>, name: &str, expected: &str) -> bool {
        hint.is_some_and(|hint| hint.eq_ignore_ascii_case(expected))
            || name.eq_ignore_ascii_case(expected)
    }

    /// Recomputes the rows after the tree changed. The cursor stays on its node
    /// when that node still has a row.
    fn refresh_visible_paths(&mut self) {
        let selected = self.selected_path();
        self.collect_rows();
        self.restore_cursor(selected);
    }

    /// Fills `visible_paths` with the rows of the tree, narrowed by the filter
    /// when it has a query, and returns where a new query puts the cursor.
    fn collect_rows(&mut self) -> FilterTargets {
        self.visible_paths.clear();
        let mut prefix = Vec::new();
        let mut targets = FilterTargets::default();

        match self.filter.as_mut().filter(|filter| filter.is_active()) {
            Some(filter) => {
                let mut guard = MATCHER.lock();
                guard.config = Config::DEFAULT;
                let mut matcher = FilterMatcher::new(filter, &mut guard);
                Self::collect_filtered_paths(
                    &self.roots,
                    &mut prefix,
                    &mut self.visible_paths,
                    &mut matcher,
                    &mut targets,
                    true,
                );
                let (mut matched, mut total) = (0, 0);
                Self::count_matches(&self.roots, &mut matcher, &mut matched, &mut total);
                filter.matched = matched;
                filter.total = total;
            }
            None => Self::collect_visible_paths(&self.roots, &mut prefix, &mut self.visible_paths),
        }

        targets
    }

    /// Puts the cursor back on the row of `selected`, or keeps it in bounds when
    /// that node has no row any more.
    fn restore_cursor(&mut self, selected: Option<Vec<usize>>) {
        if let Some(row) = selected
            .and_then(|selected| self.visible_paths.iter().position(|path| *path == selected))
        {
            self.cursor = row;
        }

        if self.visible_paths.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }

        self.cursor = self.cursor.min(self.visible_paths.len().saturating_sub(1));
        self.adjust_scroll();
    }

    /// Pushes the rows of `nodes` that the filter keeps. Below a collapsed row
    /// `shown` is false: the nodes there have no rows and only tell whether the
    /// collapsed row holds a match. Returns whether any of `nodes` matches or
    /// holds a match.
    fn collect_filtered_paths(
        nodes: &[Node],
        prefix: &mut Vec<usize>,
        visible_paths: &mut Vec<Vec<usize>>,
        matcher: &mut FilterMatcher,
        targets: &mut FilterTargets,
        shown: bool,
    ) -> bool {
        let mut holds_match = false;

        for (index, node) in nodes.iter().enumerate() {
            prefix.push(index);
            let row = visible_paths.len();
            if shown {
                visible_paths.push(prefix.clone());
            }
            let open = shown && node.expanded;

            let keep = if let Some(rank) = matcher.rank(node) {
                if shown {
                    targets.offer_match(rank, row);
                }
                if open {
                    // The rows below a match stay as the tree shows them.
                    Self::collect_matched_paths(
                        node.loaded_children(),
                        prefix,
                        visible_paths,
                        matcher,
                        targets,
                    );
                }
                true
            } else {
                let below = Self::collect_filtered_paths(
                    node.loaded_children(),
                    prefix,
                    visible_paths,
                    matcher,
                    targets,
                    open,
                );
                if below && shown && !node.expanded {
                    targets.holder.get_or_insert(row);
                }
                below
            };

            if shown && !keep {
                visible_paths.truncate(row);
            }
            prefix.pop();

            holds_match |= keep;
            if holds_match && !shown {
                break;
            }
        }

        holds_match
    }

    /// Pushes the rows below a matching node as the tree shows them, except the
    /// ones that a `!` term excludes, and offers the ones that match as well as
    /// cursor targets.
    fn collect_matched_paths(
        nodes: &[Node],
        prefix: &mut Vec<usize>,
        visible_paths: &mut Vec<Vec<usize>>,
        matcher: &mut FilterMatcher,
        targets: &mut FilterTargets,
    ) {
        for (index, node) in nodes.iter().enumerate() {
            if !matcher.passes_exclusions(node) {
                continue;
            }
            prefix.push(index);
            if let Some(rank) = matcher.rank(node) {
                targets.offer_match(rank, visible_paths.len());
            }
            visible_paths.push(prefix.clone());

            if node.expanded {
                Self::collect_matched_paths(
                    node.loaded_children(),
                    prefix,
                    visible_paths,
                    matcher,
                    targets,
                );
            }

            prefix.pop();
        }
    }

    /// Counts the loaded variables, and the ones among them that match.
    fn count_matches(
        nodes: &[Node],
        matcher: &mut FilterMatcher,
        matched: &mut usize,
        total: &mut usize,
    ) {
        for node in nodes {
            if !node.is_scope() {
                *total += 1;
                if matcher.rank(node).is_some() {
                    *matched += 1;
                }
            }
            Self::count_matches(node.loaded_children(), matcher, matched, total);
        }
    }

    /// Opens the filter line, with the query of the filter that is applied.
    fn edit_filter(&mut self) {
        self.filter.get_or_insert_with(Filter::new).editing = true;
        self.status = None;
    }

    /// Leaves the filter line. The rows stay filtered unless the query is empty.
    fn accept_filter(&mut self) {
        match &mut self.filter {
            Some(filter) if filter.is_active() => filter.editing = false,
            _ => self.clear_filter(),
        }
    }

    /// Shows all rows again, with the cursor on the node it was on.
    fn clear_filter(&mut self) {
        let Some(filter) = self.filter.take() else {
            return;
        };
        let selected = self.selected_path().or(filter.anchor);
        self.collect_rows();
        self.restore_cursor(selected);
    }

    /// Filters the rows by `input` and puts the cursor on the best match, or on
    /// the first collapsed row that holds one.
    fn set_filter_query(&mut self, input: &str) {
        let selected = self.selected_path();
        let Some(filter) = &mut self.filter else {
            return;
        };
        if !filter.set_query(input) {
            return;
        }
        if selected.is_some() {
            filter.anchor = selected;
        }

        let selected = filter.anchor.clone();
        let targets = self.collect_rows();
        match targets.best.map(|(_rank, row)| row).or(targets.holder) {
            Some(row) => {
                self.cursor = row;
                self.restore_cursor(None);
            }
            None => self.restore_cursor(selected),
        }
    }

    /// Passes an event to the filter line and filters by what it then holds.
    fn edit_filter_line(&mut self, event: &Event, ctx: &mut Context) {
        let Some(filter) = &mut self.filter else {
            return;
        };
        // Enter, Esc and Ctrl-c, which would close the prompt, never get here.
        let _ = filter.prompt.handle_event(event, ctx);
        let input = filter.prompt.line().clone();
        self.set_filter_query(&input);
    }

    /// Pastes `text` into the filter line, as one line.
    fn paste_into_filter(&mut self, text: &str, ctx: &mut Context) {
        let event = Event::Paste(sanitize_inline(text.to_string()));
        self.edit_filter_line(&event, ctx);
    }

    fn handle_filter_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(text) => {
                self.paste_into_filter(text, ctx);
                return EventResult::Consumed(None);
            }
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        match key_event {
            key!(Esc) | ctrl!('c') => self.clear_filter(),
            key!(Enter) => self.accept_filter(),
            shift!(Tab) | key!(Up) | ctrl!('p') => self.move_up(),
            key!(Tab) | key!(Down) | ctrl!('n') => self.move_down(),
            key!(PageUp) | ctrl!('u') => self.move_page_up(),
            key!(PageDown) | ctrl!('d') => self.move_page_down(),
            key!(Home) => self.move_home(),
            key!(End) => self.move_end(),
            _ => self.edit_filter_line(event, ctx),
        }

        EventResult::Consumed(None)
    }

    fn collect_visible_paths(
        nodes: &[Node],
        prefix: &mut Vec<usize>,
        visible_paths: &mut Vec<Vec<usize>>,
    ) {
        for (index, node) in nodes.iter().enumerate() {
            prefix.push(index);
            visible_paths.push(prefix.clone());

            if node.expanded {
                if let Children::Loaded(children) = &node.children {
                    Self::collect_visible_paths(children, prefix, visible_paths);
                }
            }

            prefix.pop();
        }
    }

    fn node(&self, path: &[usize]) -> Option<&Node> {
        Self::node_from_slice(&self.roots, path)
    }

    fn node_from_slice<'a>(nodes: &'a [Node], path: &[usize]) -> Option<&'a Node> {
        let (index, rest) = path.split_first()?;
        let node = nodes.get(*index)?;
        if rest.is_empty() {
            Some(node)
        } else {
            match &node.children {
                Children::Loaded(children) => Self::node_from_slice(children, rest),
                Children::Unloaded | Children::Failed(_) => None,
            }
        }
    }

    fn node_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        Self::node_mut_from_slice(&mut self.roots, path)
    }

    fn node_mut_from_slice<'a>(nodes: &'a mut [Node], path: &[usize]) -> Option<&'a mut Node> {
        let (index, rest) = path.split_first()?;
        let node = nodes.get_mut(*index)?;
        if rest.is_empty() {
            Some(node)
        } else {
            match &mut node.children {
                Children::Loaded(children) => Self::node_mut_from_slice(children, rest),
                Children::Unloaded | Children::Failed(_) => None,
            }
        }
    }

    fn selected_path(&self) -> Option<Vec<usize>> {
        self.visible_paths.get(self.cursor).cloned()
    }

    fn move_up(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor = self.cursor.saturating_sub(1);
        self.adjust_scroll();
        self.status = None;
    }

    fn move_down(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor = (self.cursor + 1).min(self.visible_paths.len().saturating_sub(1));
        self.adjust_scroll();
        self.status = None;
    }

    fn move_page_up(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor = self.cursor.saturating_sub(self.page_size.max(1));
        self.adjust_scroll();
        self.status = None;
    }

    fn move_page_down(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor =
            (self.cursor + self.page_size.max(1)).min(self.visible_paths.len().saturating_sub(1));
        self.adjust_scroll();
        self.status = None;
    }

    fn move_home(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor = 0;
        self.adjust_scroll();
        self.status = None;
    }

    fn move_end(&mut self) {
        if self.visible_paths.is_empty() {
            return;
        }

        self.cursor = self.visible_paths.len().saturating_sub(1);
        self.adjust_scroll();
        self.status = None;
    }

    fn adjust_scroll(&mut self) {
        let page_size = self.page_size.max(1);

        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + page_size {
            self.scroll = self.cursor + 1 - page_size;
        }
        // Fill the page when fewer rows than a page are left below the offset,
        // say after the filter narrowed them.
        self.scroll = self
            .scroll
            .min(self.visible_paths.len().saturating_sub(page_size));
    }

    fn expand_selected(&mut self, editor: &mut Editor) {
        let Some(path) = self.selected_path() else {
            return;
        };

        if self.expand_path(&path, editor).is_ok() {
            self.refresh_visible_paths();
        }
    }

    fn expand_path(&mut self, path: &[usize], editor: &mut Editor) -> Result<(), String> {
        let Some(node) = self.node(path) else {
            return Ok(());
        };

        if !node.is_expandable() {
            return Ok(());
        }

        if node.is_loaded() {
            if let Some(node) = self.node_mut(path) {
                node.expanded = true;
            }
            self.status = None;
            return Ok(());
        }

        let variables_reference = node.variables_reference();
        let indexed_variables = node.indexed_variables();
        match Self::load_children(
            editor,
            self.debugger_id,
            variables_reference,
            indexed_variables,
        ) {
            Ok(children) => {
                if let Some(node) = self.node_mut(path) {
                    node.children = Children::Loaded(children);
                    node.expanded = true;
                }
                self.status = None;
                Ok(())
            }
            Err(error) => {
                if let Some(node) = self.node_mut(path) {
                    node.children = Children::Failed(error.clone());
                }
                editor.set_error(error.clone());
                self.status = Some(error.clone());
                Err(error)
            }
        }
    }

    fn collapse_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };

        let collapse_current = self.node(&path).is_some_and(|node| node.expanded);
        if collapse_current {
            if let Some(node) = self.node_mut(&path) {
                node.expanded = false;
            }
            self.refresh_visible_paths();
            self.status = None;
            return;
        }

        if path.len() <= 1 {
            return;
        }

        let parent = path[..path.len() - 1].to_vec();
        if let Some(cursor) = self
            .visible_paths
            .iter()
            .position(|candidate| *candidate == parent)
        {
            self.cursor = cursor;
            self.adjust_scroll();
            self.status = None;
        }
    }

    fn load_children(
        editor: &mut Editor,
        debugger_id: DebugAdapterId,
        variables_reference: usize,
        indexed_variables: Option<usize>,
    ) -> Result<Vec<Node>, String> {
        let Some(debugger) = editor.debug_adapters.get_client(debugger_id) else {
            return Err("Debugger session ended.".to_string());
        };

        if let Some(decoded) =
            load_byte_collection(debugger, variables_reference, indexed_variables)
        {
            return Ok(vec![Node::from_decoded_bytes(decoded)]);
        }

        let response = block_on(debugger.variables(variables_reference))
            .map_err(|error| format!("Failed to load variables: {error}"))?;

        Ok(response.into_iter().map(Node::from_variable).collect())
    }

    fn styles(editor: &Editor) -> Styles {
        let theme = &editor.theme;
        Styles {
            base: theme.get("ui.popup"),
            selected: theme.get("ui.menu.selected"),
            help: theme.get("ui.help"),
            scroll: theme.get("ui.menu.scroll"),
            scope: theme.get("ui.linenr.selected"),
            name: theme.get("ui.text.focus"),
            ty: theme.get("ui.text"),
            value: theme.get("ui.text.focus"),
            matched: theme.get("special").add_modifier(Modifier::BOLD),
        }
    }

    /// `matches` holds the matched grapheme indices of each filter column when
    /// the node matches the filter.
    fn format_node(
        node: &Node,
        depth: usize,
        styles: &Styles,
        matches: Option<&[Vec<u32>; 3]>,
    ) -> Spans<'static> {
        let indices = |column: usize| matches.map_or(&[][..], |matches| &matches[column]);
        let mut spans = Vec::new();

        spans.push(Span::raw("  ".repeat(depth)));

        let marker = if node.is_expandable() {
            if node.expanded {
                "v "
            } else {
                "> "
            }
        } else {
            "  "
        };
        spans.push(Span::styled(marker.to_string(), styles.ty));

        match &node.kind {
            NodeKind::Scope {
                name, expensive, ..
            } => {
                push_highlighted(&mut spans, name, styles.scope, indices(0), styles.matched);
                if *expensive {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled("(expensive)".to_string(), styles.ty));
                }
                if let Children::Failed(error) = &node.children {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(format!("[{error}]"), styles.ty));
                }
            }
            NodeKind::Variable {
                name, value, ty, ..
            } => {
                push_highlighted(&mut spans, name, styles.name, indices(0), styles.matched);
                if let Some(ty) = ty {
                    spans.push(Span::raw(": "));
                    push_highlighted(&mut spans, ty, styles.ty, indices(1), styles.matched);
                }
                if !value.is_empty() {
                    spans.push(Span::raw(" = "));
                    push_highlighted(&mut spans, value, styles.value, indices(2), styles.matched);
                }
                if let Children::Failed(error) = &node.children {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(format!("[{error}]"), styles.ty));
                }
            }
        }

        Spans::from(spans)
    }

    /// Check if the active debug frame has changed and refresh scopes if needed.
    fn maybe_refresh(&mut self, editor: &mut Editor) {
        let current_frame_id = editor
            .debug_adapters
            .get_client(self.debugger_id)
            .and_then(|d| d.current_stack_frame())
            .map(|f| f.id);

        if current_frame_id == self.frame_id || current_frame_id.is_none() {
            return;
        }

        let frame_id = current_frame_id.unwrap();
        let Some(debugger) = editor.debug_adapters.get_client(self.debugger_id) else {
            return;
        };
        let scopes = match block_on(debugger.scopes(frame_id)) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Collect which paths (by name) were expanded so we can restore them.
        let expanded_names = self.collect_expanded_names();

        let mut sorted_scopes = scopes;
        sorted_scopes.sort_by_cached_key(Self::scope_sort_key);
        self.roots = sorted_scopes.into_iter().map(Node::from_scope).collect();
        self.frame_id = Some(frame_id);

        // Re-add watch expressions scope.
        self.build_watch_scope(editor);

        // Re-expand the "Locals" (or first non-watch) scope.
        self.refresh_visible_paths();
        let initial = self
            .roots
            .iter()
            .position(Node::is_local_scope)
            .or_else(|| (!self.roots.is_empty()).then_some(0));
        if let Some(index) = initial {
            let path = vec![index];
            let _ = self.expand_path(&path, editor);
            self.refresh_visible_paths();

            // Try to restore previously expanded child paths within this scope.
            self.restore_expanded(&expanded_names, &[index], editor);
            self.refresh_visible_paths();
        }

        self.status = None;
    }

    /// Collect the names of all expanded nodes for state restoration.
    fn collect_expanded_names(&self) -> Vec<Vec<String>> {
        let mut result = Vec::new();
        Self::collect_expanded_names_impl(&self.roots, &mut Vec::new(), &mut result);
        result
    }

    fn collect_expanded_names_impl(
        nodes: &[Node],
        prefix: &mut Vec<String>,
        result: &mut Vec<Vec<String>>,
    ) {
        for node in nodes {
            if node.expanded {
                let name = node.display_name();
                prefix.push(name);
                result.push(prefix.clone());
                if let Children::Loaded(children) = &node.children {
                    Self::collect_expanded_names_impl(children, prefix, result);
                }
                prefix.pop();
            }
        }
    }

    /// Try to re-expand nodes that match previously expanded names.
    fn restore_expanded(
        &mut self,
        expanded_names: &[Vec<String>],
        scope_path: &[usize],
        editor: &mut Editor,
    ) {
        // Collect indices to expand first, then expand them (avoids borrow conflict).
        let indices_to_expand: Vec<usize> = {
            let Some(scope_node) = self.node(scope_path) else {
                return;
            };
            let Children::Loaded(children) = &scope_node.children else {
                return;
            };
            children
                .iter()
                .enumerate()
                .filter(|(_, child)| {
                    let name = child.display_name();
                    child.is_expandable()
                        && expanded_names
                            .iter()
                            .any(|names| names.len() >= 2 && names[1] == name)
                })
                .map(|(i, _)| i)
                .collect()
        };

        for i in indices_to_expand {
            let mut child_path = scope_path.to_vec();
            child_path.push(i);
            let _ = self.expand_path(&child_path, editor);
        }
    }

    fn footer_text(&self) -> &str {
        self.status.as_deref().unwrap_or(HELP)
    }

    /// Splits the browser's area into the rows and the footer line.
    fn layout(area: Rect) -> (Rect, Rect) {
        let inner = area.inner(Margin::all(1));
        let footer_height = u16::from(inner.height > 1);
        let list_area = inner.clip_bottom(footer_height);
        let footer_area = if footer_height == 0 {
            Rect::default()
        } else {
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1)
        };
        (list_area, footer_area)
    }

    /// Draws the filter line being edited, with the match count or a hint on
    /// the right when there is room for it.
    fn render_filter_line(
        filter: &mut Filter,
        area: Rect,
        surface: &mut Surface,
        cx: &mut Context,
        style: Style,
    ) {
        if area.width < MIN_FILTER_WIDTH {
            return;
        }

        let note = if filter.is_active() {
            Cow::from(format!("{}/{}", filter.matched, filter.total))
        } else {
            Cow::from(FILTER_HINT)
        };
        let note_width = note.len() as u16;
        let show_note = area.width >= note_width + 1 + MIN_QUERY_WIDTH;

        let prompt_area = if show_note {
            area.clip_right(note_width + 1)
        } else {
            area
        };
        filter.prompt.render(prompt_area, surface, cx);
        if show_note {
            surface.set_stringn(
                area.right() - note_width,
                area.y,
                &note,
                note_width as usize,
                style,
            );
        }
        // The prompt clears its line with the editor's background.
        if let Some(bg) = style.bg {
            surface.set_style(area, Style::default().bg(bg));
        }
    }

    /// Draws the query of an applied filter, then the match count and the keys.
    fn render_applied_filter(filter: &Filter, area: Rect, surface: &mut Surface, style: Style) {
        let query = format!("/{}", sanitize_inline(filter.prompt.line().clone()));
        let rest = format!("  {}/{} | / edit | esc clear", filter.matched, filter.total);
        let query_width = area.width.saturating_sub(rest.len() as u16);
        let (x, _) = surface.set_stringn(area.x, area.y, &query, query_width as usize, style);
        surface.set_stringn(
            x,
            area.y,
            &rest,
            area.right().saturating_sub(x) as usize,
            style,
        );
    }

    fn close() -> EventResult {
        let close_fn: Callback = Box::new(|compositor: &mut Compositor, _| {
            compositor.remove(Self::ID);
        });
        EventResult::Consumed(Some(close_fn))
    }
}

impl Component for DebugVariables {
    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        if self.filter.as_ref().is_some_and(|filter| filter.editing) {
            return self.handle_filter_event(event, ctx);
        }

        let key_event = match event {
            Event::Key(event) => *event,
            // Pasted text would go to the buffer behind the browser.
            Event::Paste(text) => {
                self.edit_filter();
                self.paste_into_filter(text, ctx);
                return EventResult::Consumed(None);
            }
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        match key_event {
            key!(Esc) | ctrl!('c') if self.filter.is_some() => {
                self.clear_filter();
                EventResult::Consumed(None)
            }
            key!(Esc) | ctrl!('c') => Self::close(),
            key!('/') | shift!('/') => {
                self.edit_filter();
                EventResult::Consumed(None)
            }
            key!(Up) | ctrl!('p') | key!('k') => {
                self.move_up();
                EventResult::Consumed(None)
            }
            key!(Down) | ctrl!('n') | key!('j') => {
                self.move_down();
                EventResult::Consumed(None)
            }
            key!(PageUp) | ctrl!('u') => {
                self.move_page_up();
                EventResult::Consumed(None)
            }
            key!(PageDown) | ctrl!('d') => {
                self.move_page_down();
                EventResult::Consumed(None)
            }
            key!(Home) => {
                self.move_home();
                EventResult::Consumed(None)
            }
            key!(End) => {
                self.move_end();
                EventResult::Consumed(None)
            }
            key!(Right) | key!(Enter) | key!('l') => {
                self.expand_selected(ctx.editor);
                EventResult::Consumed(None)
            }
            key!(Left) | key!('h') => {
                self.collapse_selected();
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.viewport = viewport;
        self.page_size = viewport.1.saturating_sub(3) as usize;
        self.adjust_scroll();
        Some(viewport)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context) {
        self.maybe_refresh(ctx.editor);
        let styles = Self::styles(ctx.editor);

        surface.clear_with(area, styles.base);

        let block = Block::bordered().title(TITLE);
        block.render(area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let (list_area, footer_area) = Self::layout(area);

        self.page_size = list_area.height as usize;
        self.adjust_scroll();

        let filtering = self.filter.as_ref().is_some_and(Filter::is_active);
        if self.visible_paths.is_empty() {
            let empty = TuiText::from(if filtering { NO_MATCHES } else { EMPTY });
            Paragraph::new(&empty)
                .style(styles.base)
                .render(list_area, surface);
        } else {
            let fits = self.visible_paths.len() <= self.page_size.max(1);
            let table_area = if fits {
                list_area
            } else {
                list_area.clip_right(1)
            };

            let mut guard = MATCHER.lock();
            guard.config = Config::DEFAULT;
            let mut matcher = self
                .filter
                .as_ref()
                .filter(|filter| filter.is_active())
                .map(|filter| FilterMatcher::new(filter, &mut guard));

            let rows = self.visible_paths.iter().map(|path| {
                let depth = path.len().saturating_sub(1);
                let node = self
                    .node(path)
                    .expect("visible paths should always resolve to a node");
                let matches = matcher.as_mut().and_then(|matcher| matcher.indices(node));
                Row::new(vec![Cell::from(Self::format_node(
                    node,
                    depth,
                    &styles,
                    matches.as_ref(),
                ))])
            });

            let widths = [Constraint::Percentage(100)];
            let table = Table::new(rows)
                .style(styles.base)
                .highlight_style(styles.selected)
                .column_spacing(0)
                .widths(&widths);

            table.render_table(
                table_area,
                surface,
                &mut TableState {
                    offset: self.scroll,
                    selected: Some(self.cursor),
                },
                false,
            );

            if !fits {
                let total = self.visible_paths.len();
                let viewport = self.page_size.max(1);
                let thumb_height = viewport.pow(2).div_ceil(total).max(1).min(viewport);
                let thumb_top =
                    (viewport - thumb_height) * self.scroll / total.saturating_sub(viewport).max(1);

                for index in 0..viewport {
                    let cell =
                        &mut surface[(list_area.right() - 1, list_area.top() + index as u16)];
                    if thumb_top <= index && index < thumb_top + thumb_height {
                        cell.set_symbol("#");
                        cell.set_style(styles.scroll);
                    } else {
                        cell.set_symbol("|");
                        cell.set_style(styles.base);
                    }
                }
            }
        }

        if footer_area.height != 0 {
            let footer_style = match styles.help.bg.or(styles.base.bg) {
                Some(bg) => styles.help.bg(bg),
                None => styles.help,
            };
            match &mut self.filter {
                Some(filter) if filter.editing => {
                    Self::render_filter_line(filter, footer_area, surface, ctx, footer_style)
                }
                Some(filter) if self.status.is_none() => {
                    Self::render_applied_filter(filter, footer_area, surface, styles.help)
                }
                _ => {
                    let footer = TuiText::from(self.footer_text());
                    Paragraph::new(&footer)
                        .style(styles.help)
                        .render(footer_area, surface);
                }
            }
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let (_list_area, footer_area) = Self::layout(area);
        match &self.filter {
            Some(filter)
                if filter.editing
                    && footer_area.height != 0
                    && footer_area.width >= MIN_FILTER_WIDTH =>
            {
                filter.prompt.cursor(footer_area, editor)
            }
            _ => (None, CursorKind::Hidden),
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}

impl Filter {
    fn new() -> Self {
        Self {
            prompt: Prompt::new(
                "/".into(),
                None,
                ui::completers::none,
                |_cx: &mut Context, _input: &str, _event: PromptEvent| {},
            ),
            query: PickerQuery::new(FILTER_COLUMNS.iter().map(|&column| column.into()), 0),
            patterns: [None, None, None],
            exclusions: [None, None, None],
            editing: true,
            matched: 0,
            total: 0,
            anchor: None,
        }
    }

    /// Whether the query narrows the rows.
    fn is_active(&self) -> bool {
        self.patterns.iter().any(Option::is_some)
    }

    /// Parses `input` into a pattern per column, the way pickers read their
    /// query. Returns whether the query changed.
    fn set_query(&mut self, input: &str) -> bool {
        let old_query = self.query.parse(input);
        if self.query == old_query {
            return false;
        }

        for ((pattern, exclusion), column) in self
            .patterns
            .iter_mut()
            .zip(&mut self.exclusions)
            .zip(FILTER_COLUMNS)
        {
            *pattern = self
                .query
                .get(column)
                .map(|text| Pattern::parse(text, CaseMatching::Smart, Normalization::Smart))
                .filter(|pattern| !pattern.atoms.is_empty());
            *exclusion = pattern.as_ref().and_then(|pattern| {
                let mut exclusion = Pattern::default();
                exclusion.atoms = pattern
                    .atoms
                    .iter()
                    .filter(|atom| atom.negative)
                    .cloned()
                    .collect();
                (!exclusion.atoms.is_empty()).then_some(exclusion)
            });
        }
        true
    }
}

/// Matches nodes against the patterns of a filter.
struct FilterMatcher<'a> {
    filter: &'a Filter,
    matcher: &'a mut Matcher,
    haystack: Haystack,
}

impl<'a> FilterMatcher<'a> {
    fn new(filter: &'a Filter, matcher: &'a mut Matcher) -> Self {
        Self {
            filter,
            matcher,
            haystack: Haystack::default(),
        }
    }

    /// How well the node matches, when it is a variable and every column of
    /// the query matches it.
    fn rank(&mut self, node: &Node) -> Option<Rank> {
        if node.is_scope() {
            return None;
        }
        let mut rank = Rank { score: 0, len: 0 };
        for (pattern, text) in self.filter.patterns.iter().zip(node.filter_columns()) {
            if let Some(pattern) = pattern {
                let text = self.haystack.utf32(text);
                rank.score += pattern.score(text, self.matcher)?;
                rank.len += text.len();
            }
        }
        Some(rank)
    }

    /// Whether no `!` term of the query excludes the node.
    fn passes_exclusions(&mut self, node: &Node) -> bool {
        for (exclusion, text) in self.filter.exclusions.iter().zip(node.filter_columns()) {
            if let Some(exclusion) = exclusion {
                if exclusion
                    .score(self.haystack.utf32(text), self.matcher)
                    .is_none()
                {
                    return false;
                }
            }
        }
        true
    }

    /// The matched grapheme indices of each column, sorted, when the node matches.
    fn indices(&mut self, node: &Node) -> Option<[Vec<u32>; 3]> {
        if node.is_scope() {
            return None;
        }
        let mut matches: [Vec<u32>; 3] = Default::default();
        for ((pattern, text), indices) in self
            .filter
            .patterns
            .iter()
            .zip(node.filter_columns())
            .zip(&mut matches)
        {
            if let Some(pattern) = pattern {
                pattern.indices(self.haystack.utf32(text), self.matcher, indices)?;
                indices.sort_unstable();
                indices.dedup();
            }
        }
        Some(matches)
    }
}

impl FilterTargets {
    /// Keeps the best match; the first row wins a tie.
    fn offer_match(&mut self, rank: Rank, row: usize) {
        if self.best.is_none_or(|(best, _row)| rank.beats(best)) {
            self.best = Some((rank, row));
        }
    }
}

impl Rank {
    fn beats(self, other: Rank) -> bool {
        self.score > other.score || (self.score == other.score && self.len < other.len)
    }
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

/// Pushes `text` as spans in `style`, with `matched` patched onto the graphemes
/// at `indices`, which count graphemes (see [`Haystack`]).
fn push_highlighted(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    indices: &[u32],
    matched: Style,
) {
    if indices.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
        return;
    }

    let mut indices = indices.iter().copied().peekable();
    let mut run = String::new();
    let mut run_matched = false;
    for (index, grapheme) in text.graphemes(true).enumerate() {
        let is_match = indices.next_if_eq(&(index as u32)).is_some();
        if is_match != run_matched && !run.is_empty() {
            let run_style = if run_matched {
                style.patch(matched)
            } else {
                style
            };
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_matched = is_match;
        run.push_str(grapheme);
    }
    if !run.is_empty() {
        let run_style = if run_matched {
            style.patch(matched)
        } else {
            style
        };
        spans.push(Span::styled(run, run_style));
    }
}

impl Node {
    fn from_scope(scope: dap::Scope) -> Self {
        Self {
            kind: NodeKind::Scope {
                name: sanitize_inline(scope.name),
                presentation_hint: scope.presentation_hint.map(sanitize_inline),
                expensive: scope.expensive,
                variables_reference: scope.variables_reference,
            },
            children: if scope.variables_reference == 0 {
                Children::Loaded(Vec::new())
            } else {
                Children::Unloaded
            },
            expanded: false,
        }
    }

    fn from_variable(variable: dap::Variable) -> Self {
        Self {
            kind: NodeKind::Variable {
                name: sanitize_inline(variable.name),
                value: sanitize_inline(variable.value),
                ty: variable.ty.map(sanitize_inline),
                variables_reference: variable.variables_reference,
                indexed_variables: variable.indexed_variables,
            },
            children: if variable.variables_reference == 0 {
                Children::Loaded(Vec::new())
            } else {
                Children::Unloaded
            },
            expanded: false,
        }
    }

    fn from_decoded_bytes(decoded: crate::dap_display::DecodedBytes) -> Self {
        let label = decoded.label().to_string();
        Self {
            kind: NodeKind::Variable {
                name: "decoded".to_string(),
                value: sanitize_inline(decoded.inline),
                ty: Some(label),
                variables_reference: 0,
                indexed_variables: None,
            },
            children: Children::Loaded(Vec::new()),
            expanded: false,
        }
    }

    fn is_expandable(&self) -> bool {
        self.variables_reference() != 0
    }

    fn is_loaded(&self) -> bool {
        matches!(self.children, Children::Loaded(_) | Children::Failed(_))
    }

    fn variables_reference(&self) -> usize {
        match &self.kind {
            NodeKind::Scope {
                variables_reference,
                ..
            }
            | NodeKind::Variable {
                variables_reference,
                ..
            } => *variables_reference,
        }
    }

    fn indexed_variables(&self) -> Option<usize> {
        match &self.kind {
            NodeKind::Variable {
                indexed_variables, ..
            } => *indexed_variables,
            NodeKind::Scope { .. } => None,
        }
    }

    fn display_name(&self) -> String {
        match &self.kind {
            NodeKind::Scope { name, .. } | NodeKind::Variable { name, .. } => name.clone(),
        }
    }

    fn is_scope(&self) -> bool {
        matches!(self.kind, NodeKind::Scope { .. })
    }

    fn loaded_children(&self) -> &[Node] {
        match &self.children {
            Children::Loaded(children) => children,
            Children::Unloaded | Children::Failed(_) => &[],
        }
    }

    /// The node's text in each of [`FILTER_COLUMNS`].
    fn filter_columns(&self) -> [&str; 3] {
        match &self.kind {
            NodeKind::Scope { name, .. } => [name, "", ""],
            NodeKind::Variable {
                name, value, ty, ..
            } => [name, ty.as_deref().unwrap_or_default(), value],
        }
    }

    fn is_local_scope(&self) -> bool {
        match &self.kind {
            NodeKind::Scope {
                name,
                presentation_hint,
                ..
            } => {
                presentation_hint
                    .as_deref()
                    .is_some_and(|hint| hint.eq_ignore_ascii_case("locals"))
                    || name.eq_ignore_ascii_case("locals")
            }
            NodeKind::Variable { .. } => false,
        }
    }
}

fn sanitize_inline(text: String) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_sort_prefers_locals_first() {
        let registers = dap::Scope {
            name: "Registers".into(),
            presentation_hint: Some("registers".into()),
            variables_reference: 1,
            named_variables: None,
            indexed_variables: None,
            expensive: false,
            source: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        };
        let locals = dap::Scope {
            name: "Locals".into(),
            presentation_hint: Some("locals".into()),
            variables_reference: 1,
            named_variables: None,
            indexed_variables: None,
            expensive: false,
            source: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        };
        let arguments = dap::Scope {
            name: "Arguments".into(),
            presentation_hint: Some("arguments".into()),
            variables_reference: 1,
            named_variables: None,
            indexed_variables: None,
            expensive: false,
            source: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        };

        let mut scopes = vec![registers, arguments, locals];
        scopes.sort_by_cached_key(DebugVariables::scope_sort_key);

        assert_eq!(scopes[0].name, "Locals");
        assert_eq!(scopes[1].name, "Arguments");
        assert_eq!(scopes[2].name, "Registers");
    }

    #[test]
    fn refresh_visible_paths_includes_expanded_children() {
        let mut variables = DebugVariables {
            debugger_id: DebugAdapterId::default(),
            roots: vec![Node {
                kind: NodeKind::Scope {
                    name: "Locals".into(),
                    presentation_hint: Some("locals".into()),
                    expensive: false,
                    variables_reference: 1,
                },
                expanded: true,
                children: Children::Loaded(vec![Node {
                    kind: NodeKind::Variable {
                        name: "answer".into(),
                        value: "42".into(),
                        ty: Some("i32".into()),
                        variables_reference: 0,
                        indexed_variables: None,
                    },
                    expanded: false,
                    children: Children::Loaded(Vec::new()),
                }]),
            }],
            visible_paths: Vec::new(),
            cursor: 0,
            scroll: 0,
            page_size: 10,
            viewport: (80, 25),
            status: None,
            frame_id: None,
            filter: None,
        };

        variables.refresh_visible_paths();

        assert_eq!(variables.visible_paths, vec![vec![0], vec![0, 0]]);
    }

    fn scope(name: &str, expanded: bool, children: Vec<Node>) -> Node {
        Node {
            kind: NodeKind::Scope {
                name: name.into(),
                presentation_hint: None,
                expensive: false,
                variables_reference: 1,
            },
            children: Children::Loaded(children),
            expanded,
        }
    }

    fn variable(name: &str, ty: &str, value: &str) -> Node {
        Node {
            kind: NodeKind::Variable {
                name: name.into(),
                value: value.into(),
                ty: Some(ty.into()),
                variables_reference: 0,
                indexed_variables: None,
            },
            children: Children::Loaded(Vec::new()),
            expanded: false,
        }
    }

    fn structure(name: &str, expanded: bool, fields: Vec<Node>) -> Node {
        Node {
            kind: NodeKind::Variable {
                name: name.into(),
                value: "{...}".into(),
                ty: Some("struct".into()),
                variables_reference: 7,
                indexed_variables: None,
            },
            children: Children::Loaded(fields),
            expanded,
        }
    }

    fn browser(roots: Vec<Node>) -> DebugVariables {
        let mut variables = DebugVariables {
            debugger_id: DebugAdapterId::default(),
            roots,
            visible_paths: Vec::new(),
            cursor: 0,
            scroll: 0,
            page_size: 10,
            viewport: (80, 25),
            status: None,
            frame_id: None,
            filter: None,
        };
        variables.refresh_visible_paths();
        variables
    }

    /// The browser with a variable `resp` that holds a struct, whose fields
    /// are loaded and shown when `expanded`.
    fn response_browser(expanded: bool) -> DebugVariables {
        browser(vec![
            scope(
                "Locals",
                true,
                vec![
                    variable("err", "error", "nil"),
                    structure(
                        "resp",
                        expanded,
                        vec![
                            variable("StatusCode", "int", "404"),
                            variable("Status", "string", "\"404 Not Found\""),
                            variable("Body", "io.ReadCloser", "nil"),
                        ],
                    ),
                    variable("errCount", "int", "3"),
                    variable("buffer", "[]uint8", "[]"),
                ],
            ),
            scope("Globals", false, Vec::new()),
        ])
    }

    fn filter(variables: &mut DebugVariables, query: &str) {
        variables.edit_filter();
        variables.set_filter_query(query);
    }

    /// The names of the rows, indented by depth.
    fn rows(variables: &DebugVariables) -> Vec<String> {
        variables
            .visible_paths
            .iter()
            .map(|path| {
                let node = variables.node(path).unwrap();
                format!("{}{}", "  ".repeat(path.len() - 1), node.display_name())
            })
            .collect()
    }

    fn selected(variables: &DebugVariables) -> String {
        let path = variables.selected_path().unwrap();
        variables.node(&path).unwrap().display_name()
    }

    #[test]
    fn filter_keeps_matches_and_the_rows_above_them() {
        let mut variables = response_browser(true);
        filter(&mut variables, "statcode");

        assert_eq!(rows(&variables), ["Locals", "  resp", "    StatusCode"]);
        assert_eq!(selected(&variables), "StatusCode");
    }

    #[test]
    fn filter_keeps_the_rows_below_a_match() {
        let mut variables = response_browser(true);
        filter(&mut variables, "resp");

        assert_eq!(
            rows(&variables),
            [
                "Locals",
                "  resp",
                "    StatusCode",
                "    Status",
                "    Body"
            ]
        );
        assert_eq!(selected(&variables), "resp");
    }

    #[test]
    fn filter_selects_the_best_match() {
        let mut variables = response_browser(false);
        filter(&mut variables, "err");

        assert_eq!(rows(&variables), ["Locals", "  err", "  errCount"]);
        // Both start with the query; the shorter name wins.
        assert_eq!(selected(&variables), "err");

        filter(&mut variables, "errc");
        assert_eq!(rows(&variables), ["Locals", "  errCount"]);
        assert_eq!(selected(&variables), "errCount");
    }

    #[test]
    fn collapsed_row_that_holds_a_match_stays_collapsed() {
        let mut variables = response_browser(false);
        filter(&mut variables, "statcode");

        assert_eq!(rows(&variables), ["Locals", "  resp"]);
        assert_eq!(selected(&variables), "resp");
        assert_eq!(variables.filter.as_ref().unwrap().matched, 1);

        // Expanding it shows the match inside, and the cursor stays.
        variables.node_mut(&[0, 1]).unwrap().expanded = true;
        variables.refresh_visible_paths();
        assert_eq!(rows(&variables), ["Locals", "  resp", "    StatusCode"]);
        assert_eq!(selected(&variables), "resp");
    }

    #[test]
    fn filter_columns_match_types_and_values() {
        let mut variables = response_browser(true);

        filter(&mut variables, "%value 404");
        assert_eq!(
            rows(&variables),
            ["Locals", "  resp", "    StatusCode", "    Status"]
        );

        filter(&mut variables, "%t int");
        assert_eq!(
            rows(&variables),
            [
                "Locals",
                "  resp",
                "    StatusCode",
                "  errCount",
                "  buffer"
            ]
        );

        // Scopes have no type or value.
        filter(&mut variables, "%v Locals");
        assert!(rows(&variables).is_empty());

        // A name and a value narrow together.
        filter(&mut variables, "Status %v Found");
        assert_eq!(rows(&variables), ["Locals", "  resp", "    Status"]);
    }

    #[test]
    fn filter_counts_loaded_matches() {
        let mut variables = response_browser(false);
        filter(&mut variables, "s");

        let filter = variables.filter.as_ref().unwrap();
        // The seven variables are loaded; scopes do not count.
        assert_eq!(filter.total, 7);
        // resp, StatusCode and Status.
        assert_eq!(filter.matched, 3);
    }

    #[test]
    fn empty_query_shows_all_rows() {
        let mut variables = response_browser(false);
        let all = rows(&variables);

        filter(&mut variables, "zzz");
        assert!(rows(&variables).is_empty());

        filter(&mut variables, " ");
        assert_eq!(rows(&variables), all);
        assert!(!variables.filter.as_ref().unwrap().is_active());

        variables.accept_filter();
        assert!(variables.filter.is_none());
    }

    #[test]
    fn clearing_the_filter_keeps_the_selected_row() {
        let mut variables = response_browser(true);
        filter(&mut variables, "body");
        assert_eq!(selected(&variables), "Body");

        variables.accept_filter();
        assert!(!variables.filter.as_ref().unwrap().editing);

        variables.clear_filter();
        assert!(variables.filter.is_none());
        assert_eq!(rows(&variables).len(), 9);
        assert_eq!(selected(&variables), "Body");
    }

    #[test]
    fn filter_moves_with_the_tree() {
        // The filter applies to the rows of a new frame.
        let mut variables = response_browser(true);
        filter(&mut variables, "statcode");
        variables.accept_filter();

        variables.roots = response_browser(true).roots;
        variables.roots[0].children = Children::Loaded(vec![variable("StatusCode", "int", "200")]);
        variables.refresh_visible_paths();

        assert_eq!(rows(&variables), ["Locals", "  StatusCode"]);
        assert_eq!(selected(&variables), "StatusCode");
    }

    #[test]
    fn highlight_marks_matched_graphemes() {
        let style = Style::default();
        let matched = Style::default().add_modifier(Modifier::BOLD);
        let mut spans = Vec::new();
        push_highlighted(&mut spans, "née count", style, &[0, 1, 4], matched);

        let runs: Vec<_> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style == matched))
            .collect();
        assert_eq!(
            runs,
            [("né", true), ("e ", false), ("c", true), ("ount", false)]
        );
    }

    #[test]
    fn highlight_follows_nucleo_indices() {
        // Nucleo counts graphemes, also in text that is not ASCII.
        let pattern = Pattern::parse("nc", CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::default();
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        pattern.indices(
            Utf32Str::new("ñame cöunt", &mut buf),
            &mut matcher,
            &mut indices,
        );
        assert_eq!(indices, [0, 5]);
    }

    #[test]
    fn scope_names_do_not_match() {
        let mut variables = response_browser(true);

        filter(&mut variables, "locals");
        assert!(rows(&variables).is_empty());

        // "s" is in Locals, but only the variables that have it stay.
        filter(&mut variables, "s");
        assert_eq!(
            rows(&variables),
            [
                "Locals",
                "  resp",
                "    StatusCode",
                "    Status",
                "    Body"
            ]
        );
    }

    #[test]
    fn negations_hide_rows_below_a_match_too() {
        let mut variables = response_browser(true);

        filter(&mut variables, "%v !nil");
        assert_eq!(
            rows(&variables),
            [
                "Locals",
                "  resp",
                "    StatusCode",
                "    Status",
                "  errCount",
                "  buffer"
            ]
        );

        filter(&mut variables, "!err");
        assert_eq!(
            rows(&variables),
            [
                "Locals",
                "  resp",
                "    StatusCode",
                "    Status",
                "    Body",
                "  buffer"
            ]
        );
    }

    #[test]
    fn narrowing_the_rows_pulls_the_scroll_back() {
        let locals = (0..30)
            .map(|index| variable(&format!("v{index}"), "int", "0"))
            .chain([variable("needle", "int", "1")])
            .collect();
        let mut variables = browser(vec![scope("Locals", true, locals)]);
        variables.page_size = 10;
        variables.move_end();
        assert_eq!(variables.scroll, 22);

        filter(&mut variables, "needle");
        assert_eq!(rows(&variables), ["Locals", "  needle"]);
        // Both rows show, not only the selected one.
        assert_eq!(variables.scroll, 0);
    }

    #[test]
    fn a_query_without_matches_keeps_the_node_to_go_back_to() {
        let mut variables = response_browser(false);
        variables.move_end();
        variables.move_up();
        assert_eq!(selected(&variables), "buffer");

        filter(&mut variables, "zzz");
        assert!(variables.selected_path().is_none());
        filter(&mut variables, "");
        assert_eq!(selected(&variables), "buffer");

        filter(&mut variables, "zzz");
        variables.clear_filter();
        assert_eq!(selected(&variables), "buffer");
    }

    #[test]
    fn highlights_count_graphemes_of_decomposed_text() {
        // An `e` with a combining accent, which `Utf32Str::new` passes as bytes.
        let text = "cafe\u{301} au lait";
        let pattern = Pattern::parse("'lait", CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::default();
        let mut indices = Vec::new();
        pattern.indices(Haystack::default().utf32(text), &mut matcher, &mut indices);
        assert_eq!(indices, [8, 9, 10, 11]);

        let matched = Style::default().add_modifier(Modifier::BOLD);
        let mut spans = Vec::new();
        push_highlighted(&mut spans, text, Style::default(), &indices, matched);
        let runs: Vec<_> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style == matched))
            .collect();
        assert_eq!(runs, [("cafe\u{301} au ", false), ("lait", true)]);
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
    async fn keys_go_to_the_filter_line_or_to_the_editor() {
        let mut editor = editor();
        let mut jobs = crate::job::Jobs::new();
        let mut cx = Context {
            editor: &mut editor,
            scroll: None,
            jobs: &mut jobs,
        };
        let mut variables = response_browser(true);
        let mut press =
            |variables: &mut DebugVariables, key| variables.handle_event(&Event::Key(key), &mut cx);

        // The browser does not use `n`, so it goes on to the editor's debug menu.
        assert!(matches!(
            press(&mut variables, key!('n')),
            EventResult::Ignored(None)
        ));

        assert!(matches!(
            press(&mut variables, key!('/')),
            EventResult::Consumed(None)
        ));
        // On the filter line `n` is text like any other key.
        for key in [key!('n'), key!('a'), key!('m'), key!('e')] {
            assert!(matches!(
                press(&mut variables, key),
                EventResult::Consumed(None)
            ));
        }
        assert_eq!(variables.filter.as_ref().unwrap().prompt.line(), "name");
        assert!(rows(&variables).is_empty());

        press(&mut variables, ctrl!('w'));
        for key in [key!('b'), key!('o'), key!('d')] {
            press(&mut variables, key);
        }
        assert_eq!(rows(&variables), ["Locals", "  resp", "    Body"]);
        assert_eq!(selected(&variables), "Body");

        // Enter keeps the filter, and the tree keys work on its rows.
        press(&mut variables, key!(Enter));
        assert!(!variables.filter.as_ref().unwrap().editing);
        press(&mut variables, key!('k'));
        assert_eq!(selected(&variables), "resp");
        assert!(matches!(
            press(&mut variables, key!('x')),
            EventResult::Ignored(None)
        ));

        // Esc clears the filter, then closes the browser.
        assert!(matches!(
            press(&mut variables, key!(Esc)),
            EventResult::Consumed(None)
        ));
        assert!(variables.filter.is_none());
        assert_eq!(selected(&variables), "resp");
        assert!(matches!(
            press(&mut variables, key!(Esc)),
            EventResult::Consumed(Some(_))
        ));

        // Pasted text filters instead of going to the buffer behind, on one
        // line.
        let mut variables = response_browser(true);
        let paste = Event::Paste("err\nc".to_string());
        assert!(matches!(
            variables.handle_event(&paste, &mut cx),
            EventResult::Consumed(None)
        ));
        assert_eq!(variables.filter.as_ref().unwrap().prompt.line(), "err c");
        assert_eq!(rows(&variables), ["Locals", "  errCount"]);
    }
}

// ---------------------------------------------------------------------------
// Debug Output Panel — shows DAP output during startup in a dismissible overlay
// ---------------------------------------------------------------------------

const OUTPUT_TITLE: &str = " Debug Output ";

pub struct DebugOutputPanel {
    /// Number of log lines last frame — used to detect new output and auto-scroll.
    last_len: usize,
    /// When true, auto-close on successful init. False for manual `:debug-log`.
    auto_close: bool,
    /// Tracks whether the panel has already seen an active session (prevents
    /// immediate close if a prior session was still active).
    seen_inactive: bool,
}

impl DebugOutputPanel {
    pub const ID: &'static str = "dap-output";

    /// Panel opened during startup — will auto-close on success.
    pub fn for_startup() -> Self {
        Self {
            last_len: 0,
            auto_close: true,
            // The panel is created before the session is active, so we've
            // already "seen" the inactive state.
            seen_inactive: true,
        }
    }

    /// Panel opened manually via `:debug-log` — stays open.
    pub fn manual() -> Self {
        Self {
            last_len: 0,
            auto_close: false,
            seen_inactive: true,
        }
    }

    fn close() -> EventResult {
        let close_fn: Callback = Box::new(|compositor: &mut Compositor, _| {
            compositor.remove(Self::ID);
        });
        EventResult::Consumed(Some(close_fn))
    }
}

impl Component for DebugOutputPanel {
    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> EventResult {
        // Non-modal: only consume Esc to close, everything else passes through.
        if let Event::Key(key_event) = event {
            if matches!(*key_event, key!(Esc) | ctrl!('c')) {
                return Self::close();
            }
        }
        EventResult::Ignored(None)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        Some(viewport)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context) {
        let theme = &ctx.editor.theme;
        let base = theme.get("ui.popup");
        let text_style = theme.get("ui.text");
        let error_style = theme.get("error");
        let help_style = theme.get("ui.help");

        surface.clear_with(area, base);

        let log = &ctx.editor.debug_output_log;

        // Auto-close logic: wait until we've seen the session go from
        // inactive → active, then close if no errors.
        if self.auto_close {
            let has_active = ctx.editor.debug_adapters.get_active_client().is_some();
            if !has_active {
                self.seen_inactive = true;
            }
            if has_active && self.seen_inactive {
                let has_errors = log.iter().any(|l| l.starts_with("[stderr]"));
                if !has_errors {
                    ctx.jobs.callback(Box::pin(async {
                        Ok(crate::job::Callback::EditorCompositor(Box::new(
                            |_editor, compositor| {
                                compositor.remove("dap-output");
                            },
                        )))
                    }));
                    self.auto_close = false; // prevent repeated callbacks
                }
            }
        }

        // Auto-scroll when new lines arrive.
        let new_lines = log.len() != self.last_len;
        self.last_len = log.len();

        let block = Block::bordered().title(OUTPUT_TITLE);
        block.render(area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let footer_height: u16 = 1;
        let list_area = inner.clip_bottom(footer_height);
        let footer_area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);

        let page_size = list_area.height as usize;

        if log.is_empty() {
            surface.set_stringn(
                list_area.left(),
                list_area.top(),
                "Waiting for debug adapter...",
                list_area.width as usize,
                text_style,
            );
        } else {
            // Always show the tail (most recent output).
            let skip = if new_lines || log.len() <= page_size {
                log.len().saturating_sub(page_size)
            } else {
                log.len().saturating_sub(page_size)
            };

            for (i, line) in log.iter().skip(skip).take(page_size).enumerate() {
                let y = list_area.top() + i as u16;
                if y >= list_area.bottom() {
                    break;
                }
                let style = if line.starts_with("[stderr]") {
                    error_style
                } else {
                    text_style
                };
                surface.set_stringn(list_area.left(), y, line, list_area.width as usize, style);
            }
        }

        let has_active = ctx.editor.debug_adapters.get_active_client().is_some();
        let status = if has_active {
            "esc close | session running"
        } else {
            "esc close | starting..."
        };
        let footer = TuiText::from(status);
        Paragraph::new(&footer)
            .style(help_style)
            .render(footer_area, surface);
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}
