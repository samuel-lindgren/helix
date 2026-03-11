use crate::{
    compositor::{Callback, Component, Compositor, Context, Event, EventResult},
    ctrl, key,
};
use helix_dap::{self as dap, registry::DebugAdapterId};
use helix_lsp::block_on;
use helix_view::{
    graphics::{Margin, Rect},
    Editor,
};
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans, Text as TuiText},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Widget},
};

const TITLE: &str = " Debug Variables ";
const HELP: &str = "up/down move | right expand | left collapse | esc close";
const EMPTY: &str = "No variables available for the current frame.";

pub struct DebugVariables {
    debugger_id: DebugAdapterId,
    roots: Vec<Node>,
    visible_paths: Vec<Vec<usize>>,
    cursor: usize,
    scroll: usize,
    page_size: usize,
    viewport: (u16, u16),
    status: Option<String>,
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
    },
}

#[derive(Debug, Clone)]
enum Children {
    Unloaded,
    Loaded(Vec<Node>),
    Failed(String),
}

struct Styles {
    base: helix_view::theme::Style,
    selected: helix_view::theme::Style,
    help: helix_view::theme::Style,
    scroll: helix_view::theme::Style,
    scope: helix_view::theme::Style,
    name: helix_view::theme::Style,
    ty: helix_view::theme::Style,
    value: helix_view::theme::Style,
}

impl DebugVariables {
    pub const ID: &'static str = "dap-variables";

    pub fn new(debugger_id: DebugAdapterId, mut scopes: Vec<dap::Scope>) -> Self {
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
        };
        variables.refresh_visible_paths();
        variables
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

    fn refresh_visible_paths(&mut self) {
        self.visible_paths.clear();
        let mut prefix = Vec::new();
        Self::collect_visible_paths(&self.roots, &mut prefix, &mut self.visible_paths);

        if self.visible_paths.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }

        self.cursor = self.cursor.min(self.visible_paths.len().saturating_sub(1));
        self.adjust_scroll();
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
        match Self::load_children(editor, self.debugger_id, variables_reference) {
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
    ) -> Result<Vec<Node>, String> {
        let Some(debugger) = editor.debug_adapters.get_client(debugger_id) else {
            return Err("Debugger session ended.".to_string());
        };

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
        }
    }

    fn format_node(&self, node: &Node, depth: usize, styles: &Styles) -> Spans<'static> {
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
                spans.push(Span::styled(name.clone(), styles.scope));
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
                spans.push(Span::styled(name.clone(), styles.name));
                if let Some(ty) = ty {
                    spans.push(Span::raw(": "));
                    spans.push(Span::styled(ty.clone(), styles.ty));
                }
                if !value.is_empty() {
                    spans.push(Span::raw(" = "));
                    spans.push(Span::styled(value.clone(), styles.value));
                }
                if let Children::Failed(error) = &node.children {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(format!("[{error}]"), styles.ty));
                }
            }
        }

        Spans::from(spans)
    }

    fn footer_text(&self) -> &str {
        self.status.as_deref().unwrap_or(HELP)
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
        let key_event = match event {
            Event::Key(event) => *event,
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        match key_event {
            key!(Esc) | ctrl!('c') => Self::close(),
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
        let styles = Self::styles(ctx.editor);

        surface.clear_with(area, styles.base);

        let block = Block::bordered().title(TITLE);
        block.render(area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let footer_height = u16::from(inner.height > 1);
        let list_area = inner.clip_bottom(footer_height);
        let footer_area = if footer_height == 0 {
            Rect::default()
        } else {
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1)
        };

        self.page_size = list_area.height as usize;
        self.adjust_scroll();

        if self.visible_paths.is_empty() {
            let empty = TuiText::from(EMPTY);
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

            let rows = self.visible_paths.iter().map(|path| {
                let depth = path.len().saturating_sub(1);
                let node = self
                    .node(path)
                    .expect("visible paths should always resolve to a node");
                Row::new(vec![Cell::from(self.format_node(node, depth, &styles))])
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

        if footer_height != 0 {
            let footer = TuiText::from(self.footer_text());
            Paragraph::new(&footer)
                .style(styles.help)
                .render(footer_area, surface);
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
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
            },
            children: if variable.variables_reference == 0 {
                Children::Loaded(Vec::new())
            } else {
                Children::Unloaded
            },
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
        };

        variables.refresh_visible_paths();

        assert_eq!(variables.visible_paths, vec![vec![0], vec![0, 0]]);
    }
}
