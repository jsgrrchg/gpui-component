//! Fenced code blocks for the Markdown editor, ported from Zeron's transcript
//! code block: a rounded frame, a header with the language and a copy action,
//! and a monospace body that scrolls horizontally instead of wrapping.

use std::sync::LazyLock;

use gpui::{
    App, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, ScrollHandle,
    SharedString, StyleRefinement, Styled as _, StyledText, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use markdown::mdast::Node;

use crate::{
    ActiveTheme as _,
    clipboard::Clipboard,
    scroll::horizontal_scroll_area,
    text::{
        CodeBlock, MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, Span,
    },
};

const NAME: &str = "markdown-editor-code-block";
const HEADER_HEIGHT: f32 = 28.;
const PADDING_X: f32 = 12.;
const PADDING_Y: f32 = 10.;
/// Zeron's 18px rows at its 12.5px default code size.
const LINE_HEIGHT_RATIO: f32 = 18. / 12.5;

/// One shared registry: `TextView` reparses whenever the extension revision
/// changes, so the editor must not build a new registry on every render.
static EXTENSIONS: LazyLock<MarkdownExtensions> =
    LazyLock::new(|| MarkdownExtensions::default().plugin(CodeBlockPlugin));

pub(super) fn extensions() -> MarkdownExtensions {
    EXTENSIONS.clone()
}

struct CodeBlockData {
    lang: Option<SharedString>,
    code: SharedString,
    /// Owns the cached syntax highlighting for this block.
    block: CodeBlock,
}

struct CodeBlockPlugin;

impl MarkdownPlugin for CodeBlockPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        NAME
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Code(code) = node else {
            return None;
        };
        let lang = code
            .lang
            .as_ref()
            .filter(|lang| !lang.is_empty())
            .map(|lang| SharedString::from(lang.clone()));
        let value = SharedString::from(code.value.clone());
        Some(
            MarkdownNode::new(
                NAME,
                CodeBlockData {
                    block: CodeBlock::new(value.clone(), lang.clone(), None::<Span>),
                    lang,
                    code: value.clone(),
                },
            )
            .text(value)
            .markdown(cx.node_source(node).unwrap_or_default().to_string()),
        )
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(data) = node.data::<CodeBlockData>() else {
            return div().into_any_element();
        };
        let key = node.span.map_or(0, |span| span.start);
        let id = SharedString::from(format!("{NAME}-{key}"));
        let scroll_handle = window
            .use_keyed_state(SharedString::from(format!("{id}-scroll")), cx, |_, _| {
                ScrollHandle::new()
            })
            .read(cx)
            .clone();
        let theme = cx.theme();
        let styles = data.block.styles(&theme.highlight_theme);
        let code_size = theme.mono_font_size;

        div()
            .id(id.clone())
            .debug_selector(move || format!("{NAME}-{key}"))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(10.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.foreground.opacity(0.035))
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(PADDING_X))
                    .pr(px(5.))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.foreground.opacity(0.02))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .children(data.lang.clone()),
                    )
                    .child(
                        // Live preview reveals a block's source on mouse down;
                        // copying must leave the rendered block in place.
                        div()
                            .flex_none()
                            .debug_selector(move || format!("{NAME}-{key}-copy"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Clipboard::new(SharedString::from(format!("{id}-copy")))
                                    .value(data.code.clone())
                                    .tooltip("Copy"),
                            ),
                    ),
            )
            .child(horizontal_scroll_area(
                SharedString::from(format!("{id}-body")),
                &scroll_handle,
                &StyleRefinement::default(),
                div()
                    .min_w_full()
                    .flex_none()
                    .px(px(PADDING_X))
                    .py(px(PADDING_Y))
                    .font_family(theme.mono_font_family.clone())
                    .text_size(code_size)
                    .line_height(relative(LINE_HEIGHT_RATIO))
                    .text_color(theme.foreground)
                    .whitespace_nowrap()
                    .when(data.code.is_empty(), |this| {
                        this.min_h(code_size * LINE_HEIGHT_RATIO)
                    })
                    .child(StyledText::new(data.code.clone()).with_highlights(styles)),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_code_becomes_a_code_block_node() {
        let source = "```rust\nlet x = 1;\n```";
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
        let code = &root.children().unwrap()[0];
        let node = CodeBlockPlugin
            .parse(code, &MarkdownParseContext::new(source, 0))
            .unwrap();
        let data = node.data::<CodeBlockData>().unwrap();
        assert_eq!(data.lang.as_deref(), Some("rust"));
        assert_eq!(data.code.as_ref(), "let x = 1;");
        assert_eq!(node.as_markdown(), source);
    }
}
