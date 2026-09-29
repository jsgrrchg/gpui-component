use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
};

use gpui::{
    App, Bounds, Context, Entity, EntityInputHandler, FontStyle, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render, SharedString,
    Styled, Subscription, WeakEntity, Window, canvas, div, px, rems,
};
use gpui_base::input::{
    DisplayReplacement, EditorDisplay, EditorDisplayBlock, EditorDisplayBlockCache,
    EditorDisplayProvider, SharedEditorDisplayProvider,
};
use markdown::mdast::Node;

use super::EditorState;
use crate::{
    ActiveTheme,
    text::{TextView, TextViewStyle},
};

/// Presentation of a Markdown document. All modes share the same editor state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkdownMode {
    /// Editable Markdown source.
    #[default]
    Source,
    /// A rendered, selectable reading view.
    Preview,
    /// Editable preview: syntax is revealed around the caret and selection.
    LivePreview,
}

fn markdown_style(cx: &App) -> TextViewStyle {
    TextViewStyle {
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.theme().is_dark(),
        ..Default::default()
    }
}

pub(super) struct MarkdownReadingPreview {
    state: Entity<EditorState>,
    _subscription: Subscription,
}

pub(super) fn reading_preview(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<MarkdownReadingPreview> {
    let state = state.clone();
    window.use_keyed_state(
        ("markdown-reading-preview", state.entity_id()),
        cx,
        move |_, cx| {
            let subscription = cx.subscribe(&state, |_, _, event: &super::InputEvent, cx| {
                if matches!(event, super::InputEvent::Change) {
                    cx.notify();
                }
            });
            MarkdownReadingPreview {
                state,
                _subscription: subscription,
            }
        },
    )
}

impl Render for MarkdownReadingPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        TextView::markdown(
            SharedString::from(format!("markdown-preview-{:?}", self.state.entity_id())),
            self.state.read(cx).value(),
        )
        .style(markdown_style(cx))
        .selectable(true)
        .scrollable(true)
        .size_full()
    }
}

#[derive(Clone, Copy, Debug)]
enum Mark {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
    Highlight,
}

struct Markup {
    range: Range<usize>,
    mark: Option<Mark>,
    replacements: Vec<DisplayReplacement>,
}

struct Block {
    range: Range<usize>,
    source_start: usize,
    source: SharedString,
    tasks: Vec<Task>,
    cache: Rc<RefCell<EditorDisplayBlockCache>>,
}

#[derive(Clone)]
struct Task {
    start: usize,
    marker: Range<usize>,
}

fn collect_tasks(node: &Node, source: &str, tasks: &mut Vec<Task>) {
    if let Node::ListItem(item) = node
        && item.checked.is_some()
        && let Some(range) = node_range(node)
    {
        let first_line = source[range.clone()].split('\n').next().unwrap_or_default();
        let marker = if item.checked == Some(true) {
            first_line.find("[x]").or_else(|| first_line.find("[X]"))
        } else {
            first_line.find("[ ]")
        };
        if let Some(marker) = marker {
            let marker = range.start + marker + 1;
            tasks.push(Task {
                start: range.start,
                marker: marker..marker + 1,
            });
        }
    }
    for child in node.children().into_iter().flatten() {
        collect_tasks(child, source, tasks);
    }
}

/// Replace just escape markers and character references, leaving ordinary
/// characters as copied spans with exact source positions.
fn decoded_replacements(raw: &str, decoded: &str, start: usize) -> Vec<DisplayReplacement> {
    if raw == decoded {
        return Vec::new();
    }
    let (mut from, mut to) = (0, 0);
    let mut replacements = Vec::new();
    while from < raw.len() {
        let remaining = &raw[from..];
        if let Some(escaped) = remaining
            .strip_prefix('\\')
            .and_then(|rest| rest.chars().next())
            && escaped.is_ascii_punctuation()
            && decoded[to..].starts_with(escaped)
        {
            replacements.push(replacement(start + from..start + from + 1, ""));
            from += 1 + escaped.len_utf8();
            to += escaped.len_utf8();
            continue;
        }
        if remaining.starts_with('&')
            && let Some(end) = remaining.bytes().take(34).position(|byte| byte == b';')
        {
            let reference = &remaining[1..end];
            let value = if let Some(numeric) = reference.strip_prefix('#') {
                let (digits, radix, max_len) = if let Some(hex) = numeric.strip_prefix(['x', 'X']) {
                    (hex, 16, 6)
                } else {
                    (numeric, 10, 7)
                };
                (!digits.is_empty()
                    && digits.len() <= max_len
                    && digits.chars().all(|digit| digit.is_digit(radix)))
                .then(|| markdown::decode_numeric(digits, radix))
            } else {
                markdown::decode_named(reference, true)
            };
            if let Some(value) = value
                && decoded[to..].starts_with(&value)
                && !value.contains(['\n', '\r'])
            {
                replacements.push(replacement(
                    start + from..start + from + end + 1,
                    value.clone(),
                ));
                from += end + 1;
                to += value.len();
                continue;
            }
        }
        let character = remaining.chars().next().unwrap();
        if !decoded[to..].starts_with(character) {
            // Unsupported normalizations remain readable source, rather than
            // turning an entire paragraph into an unmappable replacement.
            return Vec::new();
        }
        from += character.len_utf8();
        to += character.len_utf8();
    }
    if to == decoded.len() {
        replacements
    } else {
        Vec::new()
    }
}

#[derive(Default)]
struct MarkdownDocument {
    markup: Vec<Markup>,
    blocks: Vec<Block>,
    links: Vec<(Range<usize>, SharedString)>,
    definitions: String,
}

fn node_range(node: &Node) -> Option<Range<usize>> {
    let position = node.position()?;
    Some(position.start.offset..position.end.offset)
}

fn line_range(source: &str, range: &Range<usize>) -> Range<usize> {
    let start = source[..range.start].rfind('\n').map_or(0, |ix| ix + 1);
    let end = source[range.end..]
        .find('\n')
        .map_or(source.len(), |ix| range.end + ix);
    start..end
}

fn touches(selection: &Range<usize>, range: &Range<usize>) -> bool {
    if selection.is_empty() {
        selection.start >= range.start && selection.start <= range.end
    } else {
        selection.start < range.end && selection.end > range.start
    }
}

fn replacement(range: Range<usize>, text: impl Into<SharedString>) -> DisplayReplacement {
    DisplayReplacement {
        range,
        text: text.into(),
    }
}

impl MarkdownDocument {
    fn parse(source: &str) -> Self {
        let mut document = Self::default();
        let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
            return document;
        };
        let definitions = root
            .children()
            .into_iter()
            .flatten()
            .filter_map(|node| {
                if let Node::Definition(definition) = node {
                    Some((definition.identifier.clone(), definition.url.clone()))
                } else {
                    None
                }
            })
            .collect::<HashMap<_, _>>();
        for node in root.children().into_iter().flatten() {
            if matches!(node, Node::Definition(_)) {
                if let Some(range) = node_range(node) {
                    document.definitions.push_str("\n\n");
                    document.definitions.push_str(&source[range]);
                }
            }
        }
        for node in root.children().into_iter().flatten() {
            document.visit(node, source, &definitions);
        }
        document
    }

    fn visit(&mut self, node: &Node, source: &str, definitions: &HashMap<String, String>) {
        let Some(range) = node_range(node) else {
            return;
        };
        let mut tasks = Vec::new();
        if matches!(
            node,
            Node::Blockquote(_) | Node::List(_) | Node::FootnoteDefinition(_)
        ) {
            collect_tasks(node, source, &mut tasks);
        }
        let rendered_block = !tasks.is_empty()
            || matches!(
                node,
                Node::Heading(_)
                    | Node::Code(_)
                    | Node::Table(_)
                    | Node::Blockquote(_)
                    | Node::ThematicBreak(_)
                    | Node::FootnoteDefinition(_)
            )
            || matches!(node, Node::Paragraph(_))
                && node.children().is_some_and(|children| {
                    children
                        .iter()
                        .any(|child| matches!(child, Node::Image(_) | Node::ImageReference(_)))
                });
        if rendered_block {
            self.blocks.push(Block {
                range: line_range(source, &range),
                source_start: range.start,
                source: format!("{}{}", &source[range], self.definitions).into(),
                tasks,
                cache: Rc::default(),
            });
            return;
        }

        if let Node::ListItem(item) = node {
            if let Some(first) = item.children.first().and_then(node_range) {
                let prefix = &source[range.start..first.start];
                if !prefix.contains('\n') && prefix.trim_start().starts_with(['-', '*', '+']) {
                    self.markup.push(Markup {
                        range: line_range(source, &(range.start..first.start)),
                        mark: None,
                        replacements: vec![replacement(range.start..first.start, "• ")],
                    });
                }
            }
        }

        let mark = match node {
            Node::Strong(_) => Some(Mark::Bold),
            Node::Emphasis(_) => Some(Mark::Italic),
            Node::Delete(_) => Some(Mark::Strike),
            Node::Link(_) | Node::LinkReference(_) => Some(Mark::Link),
            _ => None,
        };
        if let Some(mark) = mark {
            let mut replacements = Vec::new();
            if let Some(children) = node.children()
                && let (Some(first), Some(last)) = (
                    children.first().and_then(node_range),
                    children.last().and_then(node_range),
                )
            {
                if range.start < first.start {
                    replacements.push(replacement(range.start..first.start, ""));
                }
                if last.end < range.end {
                    replacements.push(replacement(last.end..range.end, ""));
                }
            }
            self.markup.push(Markup {
                range: range.clone(),
                mark: Some(mark),
                replacements,
            });
        }
        match node {
            Node::InlineCode(code) => {
                let raw = &source[range.clone()];
                let ticks = raw.chars().take_while(|ch| *ch == '`').count();
                let inner = &raw[ticks..raw.len() - ticks];
                let trim = inner.len() >= 2
                    && !raw.contains('\n')
                    && inner != code.value
                    && inner.starts_with(' ')
                    && inner.ends_with(' ')
                    && inner[1..inner.len() - 1] == code.value;
                let replacements = if raw.contains('\n') || inner == code.value || trim {
                    vec![
                        replacement(range.start..range.start + ticks + usize::from(trim), ""),
                        replacement(range.end - ticks - usize::from(trim)..range.end, ""),
                    ]
                } else {
                    vec![replacement(range.clone(), code.value.clone())]
                };
                self.markup.push(Markup {
                    range: range.clone(),
                    mark: Some(Mark::Code),
                    replacements,
                });
            }
            Node::Link(link) => self.links.push((range.clone(), link.url.clone().into())),
            Node::LinkReference(link) => {
                if let Some(url) = definitions.get(&link.identifier) {
                    self.links.push((range.clone(), url.clone().into()));
                }
            }
            Node::Text(text) => {
                // markdown-rs excludes the opening backslash from a text node
                // when its first character is escaped.
                let mut range = range.clone();
                if source[..range.start]
                    .bytes()
                    .rev()
                    .take_while(|byte| *byte == b'\\')
                    .count()
                    % 2
                    == 1
                {
                    range.start -= 1;
                }
                let raw = &source[range.clone()];
                for replacement in decoded_replacements(raw, &text.value, range.start) {
                    self.markup.push(Markup {
                        range: replacement.range.clone(),
                        mark: None,
                        replacements: vec![replacement],
                    });
                }
                self.highlights(raw, range.start);
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            for child in children {
                self.visit(child, source, definitions);
            }
        }
    }

    fn highlights(&mut self, text: &str, start: usize) {
        let mut offset = 0;
        while let Some(open) = text[offset..].find("==") {
            let from = offset + open;
            let Some(close) = text[from + 2..].find("==") else {
                break;
            };
            let to = from + 2 + close;
            if to > from + 2 && !text[from..to].contains('\n') {
                self.markup.push(Markup {
                    range: start + from..start + to + 2,
                    mark: Some(Mark::Highlight),
                    replacements: vec![
                        replacement(start + from..start + from + 2, ""),
                        replacement(start + to..start + to + 2, ""),
                    ],
                });
            }
            offset = to + 2;
        }
    }

    fn replacements(&self, selection: &Range<usize>, focused: bool) -> Vec<DisplayReplacement> {
        let active = focused || !selection.is_empty();
        let mut replacements = self
            .markup
            .iter()
            .filter(|markup| !active || !touches(selection, &markup.range))
            .flat_map(|markup| markup.replacements.iter().cloned())
            .collect::<Vec<_>>();
        replacements.sort_by_key(|replacement| (replacement.range.start, replacement.range.end));
        // Keep nested formatting markers, but discard overlapping whole-token replacements.
        let mut end = 0;
        replacements.retain(|replacement| {
            if replacement.range.start < end {
                return false;
            }
            end = replacement.range.end;
            true
        });
        replacements
    }
}

struct MarkdownDisplay {
    state: WeakEntity<EditorState>,
    text: super::Rope,
    document: MarkdownDocument,
}

pub(super) fn provider(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> SharedEditorDisplayProvider {
    let weak = state.downgrade();
    let provider =
        window.use_keyed_state(("markdown-display", state.entity_id()), cx, move |_, _| {
            Rc::new(RefCell::new(MarkdownDisplay {
                state: weak,
                text: super::Rope::new(),
                document: MarkdownDocument::default(),
            })) as SharedEditorDisplayProvider
        });
    provider.read(cx).clone()
}

impl EditorDisplayProvider for MarkdownDisplay {
    fn link_at(&self, offset: usize) -> Option<SharedString> {
        self.document
            .links
            .iter()
            .find(|(range, _)| range.contains(&offset))
            .map(|(_, url)| url.clone())
    }

    fn display(
        &mut self,
        text: &super::Rope,
        selection: Range<usize>,
        focused: bool,
        _: &Window,
        cx: &App,
    ) -> EditorDisplay {
        if !ropey::extra::esoterica::ropes_are_instances(&self.text, text) {
            let source = text.to_string();
            let caches = self
                .document
                .blocks
                .iter()
                .map(|block| (block.source.clone(), block.cache.clone()))
                .collect::<HashMap<_, _>>();
            self.document = MarkdownDocument::parse(&source);
            for block in &mut self.document.blocks {
                if let Some(cache) = caches.get(&block.source) {
                    block.cache = cache.clone();
                }
            }
            self.text = text.clone();
        }
        let mut display = EditorDisplay {
            replacements: self.document.replacements(&selection, focused),
            ..Default::default()
        };
        for markup in &self.document.markup {
            let Some(mark) = markup.mark else {
                continue;
            };
            let style = match mark {
                Mark::Bold => HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
                Mark::Italic => HighlightStyle {
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                },
                Mark::Strike => HighlightStyle {
                    strikethrough: Some(gpui::StrikethroughStyle {
                        thickness: px(1.),
                        color: None,
                    }),
                    ..Default::default()
                },
                Mark::Code => HighlightStyle {
                    background_color: Some(cx.theme().muted),
                    ..Default::default()
                },
                Mark::Link => HighlightStyle {
                    color: Some(cx.theme().primary),
                    underline: Some(gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: None,
                        wavy: false,
                    }),
                    ..Default::default()
                },
                Mark::Highlight => HighlightStyle {
                    background_color: Some(cx.theme().warning.opacity(0.25)),
                    ..Default::default()
                },
            };
            display
                .decorations
                .push(super::TextDecoration::new(markup.range.clone(), style));
        }
        for block in &self.document.blocks {
            if (focused || !selection.is_empty()) && touches(&selection, &block.range) {
                continue;
            }
            let source = block.source.clone();
            let state = self.state.clone();
            let range = block.range.clone();
            let tasks = block.tasks.clone();
            let source_start = block.source_start;
            let id = format!(
                "markdown-block-{:?}-{}",
                self.state.entity_id(),
                range.start
            );
            display.blocks.push(EditorDisplayBlock {
                range: range.clone(),
                cache: block.cache.clone(),
                render: Rc::new(move |_, cx| {
                    let task_state = state.clone();
                    let tasks = tasks.clone();
                    let view = TextView::markdown(SharedString::from(id.clone()), source.clone())
                        .style(markdown_style(cx).paragraph_gap(rems(0.25)))
                        .on_task_toggle(move |offset, checked, window, cx| {
                            if let Some(task) = tasks
                                .iter()
                                .find(|task| task.start == source_start + offset)
                                && let Some(state) = task_state.upgrade()
                            {
                                state.update(cx, |state, cx| {
                                    if !state.is_editable() {
                                        return;
                                    }
                                    let selection = state.selected_range();
                                    let marker_utf16 =
                                        state.text().byte_to_utf16_idx(task.marker.start)
                                            ..state.text().byte_to_utf16_idx(task.marker.end);
                                    state.replace_text_in_range(
                                        Some(marker_utf16),
                                        if checked { "x" } else { " " },
                                        window,
                                        cx,
                                    );
                                    state.set_selected_range(selection, cx);
                                });
                            }
                        })
                        .task_list_readonly(
                            state
                                .upgrade()
                                .is_none_or(|state| !state.read(cx).is_editable()),
                        )
                        .w_full();
                    let state = state.clone();
                    let start = range.start;
                    let end = range.end;
                    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
                    let bounds_writer = bounds.clone();
                    div()
                        .id(SharedString::from(format!("{id}-frame")))
                        .debug_selector(move || format!("markdown-live-block-{start}"))
                        .relative()
                        .w_full()
                        .py_1()
                        .text_color(cx.theme().foreground)
                        .child(view)
                        .child(
                            canvas(
                                move |measured, _, _| bounds_writer.set(measured),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        )
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            cx.stop_propagation();
                            let bounds = bounds.get();
                            let anchor = if event.position.y <= bounds.center().y {
                                start
                            } else {
                                end
                            };
                            if let Some(state) = state.upgrade() {
                                state.update(cx, |state, cx| {
                                    state.set_selected_range(anchor..anchor, cx);
                                    state.focus(window, cx);
                                });
                            }
                        })
                        .into_any_element()
                }),
            });
        }
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Context, Render, TestAppContext, VisualTestContext, point};

    struct MarkdownEditorTest {
        state: Entity<EditorState>,
        mode: MarkdownMode,
        readonly: bool,
    }

    impl Render for MarkdownEditorTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "markdown-test-editor".into())
                .w(px(500.))
                .h(px(420.))
                .child(
                    super::super::Editor::new(&self.state)
                        .markdown_mode(self.mode)
                        .readonly(self.readonly)
                        .size_full(),
                )
        }
    }

    fn editor<'a>(
        cx: &'a mut TestAppContext,
        source: &str,
    ) -> (
        Entity<MarkdownEditorTest>,
        Entity<EditorState>,
        &'a mut VisualTestContext,
    ) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("markdown")
                    .line_number(false)
                    .folding(false)
                    .default_value(source)
            });
            let content = cx.new(|_| MarkdownEditorTest {
                state,
                mode: MarkdownMode::LivePreview,
                readonly: false,
            });
            crate::Root::new(content, window, cx)
        });
        let content = view.read_with(cx, |view, _| {
            view.view()
                .clone()
                .downcast::<MarkdownEditorTest>()
                .unwrap()
        });
        let state = content.read_with(cx, |view, _| view.state.clone());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        (content, state, cx)
    }

    #[gpui::test]
    fn clicks_preserve_positions_before_and_after_entities_and_escapes(cx: &mut TestAppContext) {
        cx.update(crate::init);
        for (source, offset) in [
            ("abcdefghijklmnopqrstuvwxyz &amp; suffix", 9),
            ("before &amp; suffix", 16),
            ("niño &#x4e16; suffix", 17),
            ("before \\*literal\\* suffix", 12),
            ("\\*literal\\* suffix", 5),
            ("before `` code `` suffix", 12),
        ] {
            let (_, state, cx) = editor(cx, source);
            let bounds = state
                .read_with(cx, |state, _| state.range_to_bounds(&(offset..offset)))
                .unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            assert_eq!(
                state.read_with(cx, |state, _| state.selected_range()),
                offset..offset,
                "{source:?}"
            );
            assert_eq!(
                state.read_with(cx, |state, _| state.value()).as_ref(),
                source
            );
        }
    }

    #[gpui::test]
    fn block_clicks_choose_top_or_bottom_source_anchors(cx: &mut TestAppContext) {
        cx.update(crate::init);
        for bottom in [false, true] {
            let source = "| A | B |\n| - | - |\n| a | b |\n| c | d |\n\nend";
            let (_, state, cx) = editor(cx, source);
            let bounds = cx.debug_bounds("markdown-live-block-0").unwrap();
            let y = if bottom {
                bounds.bottom() - px(2.)
            } else {
                bounds.origin.y + px(2.)
            };
            cx.simulate_click(
                point(bounds.origin.x + px(12.), y),
                gpui::Modifiers::default(),
            );
            cx.run_until_parked();
            let expected = if bottom {
                source.find("\n\n").unwrap()
            } else {
                0
            };
            assert_eq!(
                state.read_with(cx, |state, _| state.selected_range()),
                expected..expected
            );
            cx.update(|window, cx| window.draw(cx).clear());
            assert!(cx.debug_bounds("markdown-live-block-0").is_none());
        }
    }

    #[gpui::test]
    fn nested_and_quoted_tasks_toggle_independently_with_undo_and_readonly(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        for source in [
            "- [ ] Parent\n  - [ ] Child\n\nend",
            "- [ ] niño 世界\n  - [ ] hija 世界\n\nend",
            "> - [ ] Parent\n>   - [x] Child\n\nend",
            "- plain\n- [X] Done [ ] literal\n\nend",
        ] {
            let document = MarkdownDocument::parse(source);
            let block = document.blocks.first().unwrap();
            let tasks = block.tasks.clone();
            let source_start = block.source_start;
            let (content, state, cx) = editor(cx, source);
            let mut expected = source.to_owned();
            for task in &tasks {
                cx.update(|window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_selected_range(source.len()..source.len(), cx);
                        state.focus(window, cx);
                    })
                });
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear());
                // GPUI's test lookup requires a static selector.
                let selector = Box::leak(
                    format!("markdown-task-{}", task.start - source_start).into_boxed_str(),
                );
                let checkbox = cx
                    .debug_bounds(selector)
                    .expect("every nested task must have a control");
                let before = expected.clone();
                let checked = &expected[task.marker.clone()] == " ";
                expected.replace_range(task.marker.clone(), if checked { "x" } else { " " });
                cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    expected
                );
                assert_eq!(
                    state.read_with(cx, |state, _| state.selected_range()),
                    source.len()..source.len()
                );
                cx.dispatch_action(super::super::Undo);
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    before
                );
                cx.dispatch_action(super::super::Redo);
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    expected
                );
            }
            cx.update(|window, cx| {
                state.update(cx, |state, cx| {
                    state.set_selected_range(source.len()..source.len(), cx);
                    state.focus(window, cx);
                })
            });
            content.update(cx, |view, cx| {
                view.readonly = true;
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear());
            for task in &tasks {
                let selector = Box::leak(
                    format!("markdown-task-{}", task.start - source_start).into_boxed_str(),
                );
                let checkbox = cx.debug_bounds(selector).unwrap();
                cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    expected
                );
            }
        }
    }

    #[gpui::test]
    fn enter_continues_markdown_and_remains_undoable_in_both_editable_modes(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        for mode in [MarkdownMode::Source, MarkdownMode::LivePreview] {
            for (source, expected, shift) in [
                ("- item", "- item\n- ", false),
                ("12. item", "12. item\n13. ", false),
                ("- [x] niño 世界", "- [x] niño 世界\n- [ ] ", false),
                ("> - item", "> - item\n> - ", false),
                ("- item\n- ", "- item\n", false),
                ("- [ ] ", "", false),
                ("- item", "- item\n", true),
                ("```\n- literal", "```\n- literal\n", false),
            ] {
                let (content, state, cx) = editor(cx, source);
                content.update(cx, |view, cx| {
                    view.mode = mode;
                    cx.notify();
                });
                cx.update(|window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_selected_range(source.len()..source.len(), cx);
                        state.focus(window, cx);
                    })
                });
                cx.run_until_parked();
                cx.dispatch_action(super::super::Enter {
                    secondary: false,
                    shift,
                });
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    expected,
                    "{mode:?}: {source:?}"
                );
                cx.dispatch_action(super::super::Undo);
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    source
                );
                cx.dispatch_action(super::super::Redo);
                cx.run_until_parked();
                assert_eq!(
                    state.read_with(cx, |state, _| state.value()).as_ref(),
                    expected
                );
            }
        }
    }

    #[test]
    fn task_blocks_retain_all_nested_source_markers() {
        for source in [
            "- [ ] Parent\n  - [x] Child",
            "> - [ ] Parent\n>   - [X] Child",
        ] {
            let document = MarkdownDocument::parse(source);
            assert_eq!(document.blocks.len(), 1);
            assert_eq!(document.blocks[0].tasks.len(), 2);
            for task in &document.blocks[0].tasks {
                assert!(matches!(&source[task.marker.clone()], " " | "x" | "X"));
            }
        }
    }

    #[test]
    fn references_and_escapes_are_decoded_without_replacing_plain_text() {
        for (source, expected) in [
            ("A &amp; B &#x4e16; &#30028;", "A & B 世 界"),
            ("A \\*literal\\* B", "A *literal* B"),
            ("\\&amp; &NotEqualTilde;", "&amp; ≂̸"),
            ("==A &amp; B==", "A & B"),
        ] {
            assert_eq!(
                projected(source, 0..0, false),
                expected,
                "{:#?}",
                markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap()
            );
        }
        let document = MarkdownDocument::parse("ordinary text &amp; suffix");
        let replacements = document.replacements(&(0..0), false);
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].range, 14..19);
    }

    #[gpui::test]
    fn rendered_blocks_reveal_on_click_and_modes_keep_the_document(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| EditorState::new(window, cx).language("markdown").line_number(false).folding(false)
                .default_value("# Heading\n\n**niño 世界**\n\n```rust\nlet n = 1;\n```\n\n| A | B |\n| - | - |\n| a | b |\n"));
            let content = cx.new(|_| MarkdownEditorTest { state, mode: MarkdownMode::LivePreview, readonly: false });
            crate::Root::new(content, window, cx)
        });
        let content = view.read_with(cx, |view, _| {
            view.view()
                .clone()
                .downcast::<MarkdownEditorTest>()
                .unwrap()
        });
        let state = content.read_with(cx, |view, _| view.state.clone());
        cx.run_until_parked();
        let source = state.read_with(cx, |state, _| state.value());
        cx.update(|window, cx| window.draw(cx).clear());
        let heading = cx
            .debug_bounds("markdown-live-block-0")
            .expect("heading must be rendered");
        cx.simulate_click(heading.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("markdown-live-block-0").is_none());
        assert_eq!(state.read_with(cx, |state, _| state.selected_range()), 0..0);
        for mode in [
            MarkdownMode::Source,
            MarkdownMode::Preview,
            MarkdownMode::LivePreview,
        ] {
            content.update(cx, |view, cx| {
                view.mode = mode;
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(state.read_with(cx, |state, _| state.value()), source);
        }
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.focus(window, cx);
                state.set_selected_range(14..14, cx);
                state.replace_text_in_range(None, "edit ", window, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            state
                .read_with(cx, |state, _| state.value())
                .contains("edit ")
        );
        content.update(cx, |view, cx| {
            view.mode = MarkdownMode::Source;
            cx.notify();
        });
        cx.run_until_parked();
        cx.dispatch_action(super::super::Undo);
        cx.run_until_parked();
        assert_eq!(state.read_with(cx, |state, _| state.value()), source);
        cx.dispatch_action(super::super::Redo);
        cx.run_until_parked();
        assert!(
            state
                .read_with(cx, |state, _| state.value())
                .contains("edit ")
        );
    }

    #[gpui::test]
    fn task_checkboxes_update_source_and_respect_readonly(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("markdown")
                    .line_number(false)
                    .folding(false)
                    .default_value("- [ ] Task\n\nend")
            });
            let content = cx.new(|_| MarkdownEditorTest {
                state,
                mode: MarkdownMode::LivePreview,
                readonly: false,
            });
            crate::Root::new(content, window, cx)
        });
        let content = view.read_with(cx, |view, _| {
            view.view()
                .clone()
                .downcast::<MarkdownEditorTest>()
                .unwrap()
        });
        let state = content.read_with(cx, |view, _| view.state.clone());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let checkbox = cx
            .debug_bounds("markdown-live-task-checkbox")
            .expect("task checkbox must be rendered");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "- [x] Task\n\nend"
        );
        content.update(cx, |view, cx| {
            view.readonly = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let checkbox = cx
            .debug_bounds("markdown-live-task-checkbox")
            .expect("read-only task remains visible");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "- [x] Task\n\nend"
        );
    }

    fn projected(source: &str, selection: Range<usize>, focused: bool) -> String {
        let document = MarkdownDocument::parse(source);
        let mut output = source.to_string();
        for replacement in document.replacements(&selection, focused).into_iter().rev() {
            output.replace_range(replacement.range, &replacement.text);
        }
        output
    }

    #[test]
    fn syntax_is_revealed_only_for_the_active_token() {
        let source = "**bold** and *italic* and `code`";
        assert_eq!(
            projected(source, 2..2, true),
            "**bold** and italic and code"
        );
        assert_eq!(projected(source, 9..9, true), "bold and italic and code");
        assert_eq!(projected(source, 0..0, false), "bold and italic and code");
    }

    #[test]
    fn selections_reveal_all_intersecting_markup() {
        let source = "**uno** y **dos**";
        assert_eq!(projected(source, 3..14, true), source);
        assert_eq!(projected(source, 3..14, false), source);
    }

    #[test]
    fn nested_marks_links_unicode_and_highlights() {
        let source = "**_niño 世界_** [enlace](https://example.org) ~~old~~ ==new==";
        assert_eq!(
            projected(source, source.len()..source.len(), false),
            "niño 世界 enlace old new"
        );
    }

    #[test]
    fn code_and_escaped_markup_are_not_reparsed_as_formatting() {
        assert_eq!(
            projected("`**literal**` \\*text\\*", 0..0, false),
            "**literal** *text*"
        );
        assert_eq!(projected("`one\ntwo`", 0..0, false), "one\ntwo");
    }

    #[test]
    fn blocks_and_tasks_keep_source_ranges() {
        let source = "# Title\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n| - | - |\n| a | b |\n\n- [ ] Task\n";
        let document = MarkdownDocument::parse(source);
        assert_eq!(document.blocks.len(), 4);
        let task = document.blocks.last().unwrap().tasks.first().unwrap();
        assert_eq!(&source[task.marker.clone()], " ");
        for block in document.blocks {
            assert!(block.range.start == 0 || source.as_bytes()[block.range.start - 1] == b'\n');
            assert!(block.range.end == source.len() || source.as_bytes()[block.range.end] == b'\n');
        }
    }
}
