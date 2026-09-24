use crate::ui::{
    document::{LinePos, TextRenderer},
    text_decorations::Decoration,
};
use helix_core::{doc_formatter::FormattedGrapheme, Position};
use helix_view::{
    review::{Block, Layout},
    theme::Style,
    Theme,
};

pub struct Reviews<'a> {
    layout: Layout<'a>,
    style: Style,
}
impl<'a> Reviews<'a> {
    pub fn new(blocks: &'a [Block], width: u16, theme: &Theme) -> Self {
        Self {
            layout: Layout::new(blocks, width),
            style: theme.get("ui.virtual.inlay-hint"),
        }
    }
}
impl Decoration for Reviews<'_> {
    fn decorate_virtual_text(&self) -> bool {
        false
    }
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.layout.reset(pos)
    }
    fn decorate_grapheme(&mut self, _: &mut TextRenderer, _: &FormattedGrapheme) -> usize {
        self.layout.anchor()
    }
    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        offset: Position,
    ) -> Position {
        let rows = self.layout.take_rows();
        for (i, text) in rows.iter().enumerate() {
            let row = usize::from(pos.visual_line) + offset.row + i;
            renderer.draw_virtual_text(row, &format!("│ {text}"), self.style);
        }
        Position::new(rows.len(), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{document::render_text, text_decorations::DecorationManager};
    use arc_swap::ArcSwap;
    use helix_core::{
        doc_formatter::{DocumentFormatter, TextFormat},
        syntax,
        text_annotations::TextAnnotations,
        Rope,
    };
    use helix_view::{
        editor::Config,
        graphics::Rect,
        review::{Comment, Thread},
        Document,
    };
    use std::sync::Arc;
    use tui::buffer::Buffer;

    fn render(
        width: u16,
        offset: usize,
        expanded: bool,
        soft_wrap: bool,
        extra_block: bool,
        hint: bool,
    ) -> Vec<String> {
        let text = Rope::from("abcdefghijklmnop\nnext\n");
        let thread = Arc::new(Thread {
            id: "1".into(),
            path: "a.rs".into(),
            lines: Some(1..2),
            location: "a.rs:1".into(),
            resolved: false,
            diff: String::new(),
            url: String::new(),
            comments: vec![
                Comment {
                    author: "reviewer".into(),
                    body: "review body 界\nsecond paragraph".into(),
                },
                Comment {
                    author: "reply".into(),
                    body: "reply body".into(),
                },
            ],
            ..Default::default()
        });
        let block = Block {
            thread,
            range: 0..17,
            anchor: 16,
            expanded,
        };
        let mut blocks = vec![block.clone()];
        if extra_block {
            blocks.push(block);
        }
        let format = TextFormat {
            soft_wrap,
            viewport_width: width,
            wrap_indicator: "".into(),
            ..TextFormat::default()
        };
        let inline = [helix_core::text_annotations::InlineAnnotation::new(
            16,
            " very long inlay hint",
        )];
        let mut annotations = TextAnnotations::default();
        if hint {
            annotations.add_inline_annotations(&inline, None);
        }
        annotations.add_line_annotation(Box::new(Layout::new(&blocks, width)));
        let next_line =
            DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 0)
                .find(|g| g.char_idx == 17)
                .unwrap()
                .visual_pos
                .row;
        let mut empty_annotations = TextAnnotations::default();
        if hint {
            empty_annotations.add_inline_annotations(&inline, None);
        }
        let code_rows = DocumentFormatter::new_at_prev_checkpoint(
            text.slice(..),
            &format,
            &empty_annotations,
            0,
        )
        .find(|g| g.char_idx == 17)
        .unwrap()
        .visual_pos
        .row;
        assert_eq!(
            next_line,
            code_rows
                + blocks
                    .iter()
                    .map(|b| helix_view::review::rows(b, width).len())
                    .sum::<usize>()
        );
        let doc = Document::from(
            text.clone(),
            None,
            Arc::new(ArcSwap::from_pointee(Config::default())),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        let theme = Theme::default();
        let area = Rect::new(0, 0, width, 35);
        let mut buffer = Buffer::empty(area);
        let mut renderer =
            TextRenderer::new(&mut buffer, &doc, &theme, Position::new(offset, 0), area);
        let mut decorations = DecorationManager::default();
        decorations.add_decoration(Reviews::new(&blocks, width, &theme));
        render_text(
            &mut renderer,
            text.slice(..),
            0,
            &format,
            &annotations,
            None,
            vec![],
            &theme,
            decorations,
        );
        (0..area.height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol.as_str())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn blocks_reserve_and_draw_below_code_with_scroll_wrap_and_multiple_threads() {
        let rows = render(60, 0, false, false, false, false);
        assert!(rows[0].starts_with("abcdefghijklmnop"));
        assert!(rows[1].contains("[+] @reviewer"));
        assert!(rows[2].contains("review body"));
        assert!(rows[3].starts_with("next"));
        let scrolled = render(60, 2, false, false, false, false);
        assert!(scrolled[0].contains("review body"));
        assert!(scrolled[1].starts_with("next"));
        let expanded = render(60, 0, true, false, true, false);
        assert!(expanded.iter().any(|r| r.contains("reply body")));
        assert_eq!(
            expanded
                .iter()
                .filter(|r| r.contains("[-] @reviewer"))
                .count(),
            2
        );
        let wrapped = render(8, 0, false, true, false, false);
        assert!(wrapped[0].starts_with("abcdefgh"));
        assert!(wrapped[1].starts_with("ijklmnop"));
        assert!(wrapped[3].starts_with("│ [+]"));
        let hints = render(8, 0, false, true, false, true);
        let hint_row = hints.iter().position(|row| row.contains("hint")).unwrap();
        let comment_row = hints
            .iter()
            .position(|row| row.starts_with("│ [+]"))
            .unwrap();
        assert!(hint_row < comment_row, "{hints:?}");
        render(1, 0, true, true, true, false); // tiny split and wide characters must not panic
    }
}
