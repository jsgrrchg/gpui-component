use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
};

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Context, Edges, Entity, EntityInputHandler, FontStyle,
    FontWeight, HighlightStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Render, SharedString, Styled, Subscription, TransformationMatrix,
    WeakEntity, Window, canvas, div, point, px, quad, rems, size,
};
use gpui_base::input::{
    DisplayReplacement, EditorDisplay, EditorDisplayBlock, EditorDisplayBlockCache,
    EditorDisplayProvider, SharedEditorDisplayProvider,
};
use markdown::mdast::Node;

use super::EditorState;
use crate::text::incremental::{MarkdownEdit, MarkdownIndex, ReparsedMarkdown};
use crate::{
    ActiveTheme, IconName, IconNamed as _,
    text::{MarkdownNotes, TableAppearance, TextView, TextViewState, TextViewStyle},
};

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);

mod code_block;
mod code_editor;

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

fn markdown_style(
    cx: &App,
    image_root: &Option<PathBuf>,
    notes: &Option<MarkdownNotes>,
) -> TextViewStyle {
    TextViewStyle {
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.theme().is_dark(),
        table_appearance: TableAppearance::Plain,
        image_root: image_root.clone(),
        notes: notes.clone(),
        ..Default::default()
    }
}

pub(super) struct MarkdownReadingPreview {
    state: Entity<EditorState>,
    _subscription: Subscription,
    _observation: Subscription,
    text: super::Rope,
    view: Entity<TextViewState>,
    content_padding: Option<Edges<Pixels>>,
}

impl MarkdownReadingPreview {
    pub(super) fn set_content_padding(
        &mut self,
        padding: Option<Edges<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.content_padding != padding {
            self.content_padding = padding;
            cx.notify();
        }
    }
}

/// Window y of the top of the editor's [`EditorState::top_inset`] in `mode`,
/// where a host lays a document header that scrolls with the text. `None`
/// before the view has been laid out.
pub fn top_inset_y(
    state: &Entity<EditorState>,
    mode: MarkdownMode,
    window: &mut Window,
    cx: &mut App,
) -> Option<Pixels> {
    let _ = window;
    if mode == MarkdownMode::Preview {
        let preview = cx
            .try_global::<ReadingPreviews>()?
            .0
            .get(&state.entity_id())?
            .upgrade()?;
        let preview = preview.read(cx);
        let top = preview
            .content_padding
            .map_or(px(0.), |padding| padding.top);
        let (viewport, scroll) = preview.view.read(cx).list_scroll();
        return (viewport.size.height > px(0.)).then(|| viewport.top() + top + scroll.y);
    }
    let state = state.read(cx);
    state.line_height()?;
    Some(state.input_bounds().top() + state.scroll_offset().y)
}

/// Reading views by editor, so hosts can query their scroll outside a draw.
#[derive(Default)]
struct ReadingPreviews(HashMap<gpui::EntityId, WeakEntity<MarkdownReadingPreview>>);

impl gpui::Global for ReadingPreviews {}

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
            let preview = cx.entity().downgrade();
            let previews = &mut cx.default_global::<ReadingPreviews>().0;
            previews.retain(|_, preview| preview.upgrade().is_some());
            previews.insert(state.entity_id(), preview);
            let subscription = cx.subscribe(&state, |_, _, event: &super::InputEvent, cx| {
                if matches!(event, super::InputEvent::Change) {
                    cx.notify();
                }
            });
            MarkdownReadingPreview {
                _observation: cx.observe(&state, |_, _, cx| cx.notify()),
                state,
                text: super::Rope::new(),
                view: gpui::AppContext::new(cx, |cx| TextViewState::markdown("", cx)),
                content_padding: None,
                _subscription: subscription,
            }
        },
    )
}

impl Render for MarkdownReadingPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let display = display_state(&self.state, window, cx);
        let (image_root, notes) = {
            let display = display.borrow();
            (display.image_root.clone(), display.notes.clone())
        };
        let text = self.state.read(cx).text().clone();
        if !ropey::extra::esoterica::ropes_are_instances(&self.text, &text) {
            let source = text.to_string();
            self.text = text;
            self.view.update(cx, |view, cx| view.set_text(&source, cx));
        }
        // The list's vertical padding scrolls with its rows, so the top
        // inset is extra top padding here.
        let inset = self.state.read(cx).top_inset();
        let padding = self.content_padding.unwrap_or_default();
        TextView::new(&self.view)
            .style(markdown_style(cx, &image_root, &notes))
            .markdown_extensions(code_block::extensions())
            .selectable(true)
            .scrollable(true)
            .content_padding(Edges {
                top: padding.top + inset,
                ..padding
            })
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

#[derive(Clone)]
struct Markup {
    range: Range<usize>,
    mark: Option<Mark>,
    replacements: Vec<DisplayReplacement>,
}

#[derive(Clone)]
struct Block {
    id: u64,
    range: Range<usize>,
    source_start: usize,
    source: SharedString,
    tasks: Vec<Task>,
    code: Option<code_editor::FencedCode>,
    /// Includes code fences not eligible for a nested editable code view.
    code_fence: bool,
    cache: Rc<RefCell<EditorDisplayBlockCache>>,
}

/// Room reserved in the text for a task's checkbox, which is painted over it.
/// Font glyphs such as U+2610 and U+2611 are clipped or missing in some fonts.
const TASK_PLACEHOLDER: &str = "\u{2003}\u{2002}";
/// Zeron's checkbox, scaled down to fit a text line.
const TASK_CHECKBOX_SIZE: f32 = 14.;

#[derive(Clone)]
struct Task {
    /// The list item's start.
    start: usize,
    /// The ` `, `x` or `X` between the brackets.
    marker: Range<usize>,
    /// Source shown as a checkbox glyph: the bullet through `]`, or just the
    /// brackets after an ordered list number.
    glyph: Range<usize>,
    /// The item's first line, which reveals the source while it is active.
    line: Range<usize>,
    checked: bool,
}

impl Task {
    fn of(node: &Node, source: &str) -> Option<Self> {
        let Node::ListItem(item) = node else {
            return None;
        };
        let checked = item.checked?;
        let range = node_range(node)?;
        let first_line = source[range.clone()].split('\n').next().unwrap_or_default();
        let marker = if checked {
            first_line.find("[x]").or_else(|| first_line.find("[X]"))
        } else {
            first_line.find("[ ]")
        }?;
        let marker = range.start + marker + 1;
        let bullet = matches!(source.as_bytes()[range.start], b'-' | b'*' | b'+');
        Some(Self {
            start: range.start,
            marker: marker..marker + 1,
            glyph: if bullet { range.start } else { marker - 1 }..marker + 2,
            line: line_range(source, &(range.start..marker)),
            checked,
        })
    }
}

fn collect_tasks(node: &Node, source: &str, tasks: &mut Vec<Task>) {
    tasks.extend(Task::of(node, source));
    for child in node.children().into_iter().flatten() {
        collect_tasks(child, source, tasks);
    }
}

/// Set a task's marker in the document, keeping its selection.
fn set_task(
    state: &Entity<EditorState>,
    marker: &Range<usize>,
    checked: bool,
    window: &mut Window,
    cx: &mut App,
) {
    state.update(cx, |state, cx| {
        if !state.is_editable() {
            return;
        }
        let selection = state.selected_range();
        let marker_utf16 = state.text().byte_to_utf16_idx(marker.start)
            ..state.text().byte_to_utf16_idx(marker.end);
        state.replace_text_in_range(
            Some(marker_utf16),
            if checked { "x" } else { " " },
            window,
            cx,
        );
        state.set_selected_range(selection, cx);
        // The checkbox has its own dispatch focus. Return it to the document
        // so Undo/Redo continues to target the edit just performed.
        state.focus(window, cx);
    });
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

#[derive(Clone, Default)]
struct MarkdownDocument {
    markup: Vec<Markup>,
    blocks: Vec<Block>,
    links: Vec<(Range<usize>, SharedString)>,
    definitions: String,
    fenced_code_blocks: usize,
    /// Tasks of lists edited line by line, outside rendered blocks.
    tasks: Vec<Task>,
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

fn shift_range(range: &mut Range<usize>, delta: isize) {
    range.start = range
        .start
        .checked_add_signed(delta)
        .expect("Markdown range start");
    range.end = range
        .end
        .checked_add_signed(delta)
        .expect("Markdown range end");
}

impl Task {
    fn shift(&mut self, delta: isize) {
        self.start = self
            .start
            .checked_add_signed(delta)
            .expect("Markdown task start");
        shift_range(&mut self.marker, delta);
        shift_range(&mut self.glyph, delta);
        shift_range(&mut self.line, delta);
    }
}

impl Block {
    fn shift(&mut self, delta: isize) {
        shift_range(&mut self.range, delta);
        self.source_start = self
            .source_start
            .checked_add_signed(delta)
            .expect("Markdown block start");
        for task in &mut self.tasks {
            task.shift(delta);
        }
        if let Some(code) = &mut self.code {
            shift_range(&mut code.content, delta);
        }
    }
}

impl Markup {
    fn shift(&mut self, delta: isize) {
        shift_range(&mut self.range, delta);
        for replacement in &mut self.replacements {
            shift_range(&mut replacement.range, delta);
        }
    }
}

fn splice_items<T>(
    items: &mut Vec<T>,
    replacements: Vec<T>,
    range: &Range<usize>,
    delta: isize,
    start: impl Fn(&T) -> usize,
    shift: impl Fn(&mut T, isize),
) {
    items.retain_mut(|item| {
        let offset = start(item);
        if range.contains(&offset) {
            return false;
        }
        if offset >= range.end {
            shift(item, delta);
        }
        true
    });
    items.extend(replacements);
}

impl MarkdownDocument {
    /// Preserve unaffected presentation and its geometry while parsing catches
    /// up. Dirty tokens show their editable source instead of stale mappings.
    fn project_edit(&mut self, text: &super::Rope, range: &Range<usize>, new_len: usize) {
        let delta = new_len as isize - range.len() as isize;
        let unaffected = |item: &mut Range<usize>| {
            if item.end < range.start {
                return true;
            }
            if item.start > range.end {
                shift_range(item, delta);
                return true;
            }
            false
        };
        self.markup.retain_mut(|item| {
            if item.range.end < range.start {
                true
            } else if item.range.start > range.end {
                item.shift(delta);
                true
            } else {
                false
            }
        });
        self.links.retain_mut(|(item, _)| unaffected(item));
        self.tasks.retain_mut(|task| {
            if task.line.end < range.start {
                true
            } else if task.line.start > range.end {
                task.shift(delta);
                true
            } else {
                false
            }
        });
        self.blocks.retain_mut(|block| {
            if block.range.end < range.start {
                return true;
            }
            if block.range.start > range.end {
                block.shift(delta);
                return true;
            }
            if let Some(code) = &mut block.code
                && range.start >= code.content.start
                && range.end <= code.content.end
            {
                block.range.end = block.range.end.checked_add_signed(delta).unwrap();
                code.content.end = code.content.end.checked_add_signed(delta).unwrap();
                let raw = text.slice(block.range.clone()).to_string();
                let lines = text.slice(code.content.clone()).to_string();
                let value = lines.strip_suffix('\n').unwrap_or(&lines);
                let Some(mut updated) = code_editor::fenced_code(
                    &raw,
                    &(0..raw.len()),
                    code.lang.as_deref(),
                    value,
                    code.ordinal,
                ) else {
                    return false;
                };
                shift_range(&mut updated.content, block.range.start as isize);
                block.source = (raw + &self.definitions).into();
                block.code = Some(updated);
                block.cache = Rc::default();
                return true;
            }
            false
        });
    }

    fn reuse_blocks<'a>(&mut self, previous: impl IntoIterator<Item = &'a Block>) {
        let mut caches: HashMap<SharedString, VecDeque<_>> = HashMap::new();
        for block in previous {
            caches
                .entry(block.source.clone())
                .or_default()
                .push_back((block.id, block.cache.clone()));
        }
        for block in &mut self.blocks {
            if let Some((id, cache)) = caches.get_mut(&block.source).and_then(VecDeque::pop_front) {
                block.id = id;
                block.cache = cache;
            }
        }
    }

    fn splice(&mut self, reparsed: ReparsedMarkdown) {
        let mut fragment = Self::from_root(&reparsed.source, &reparsed.root);
        fragment.reuse_blocks(
            self.blocks
                .iter()
                .filter(|block| reparsed.old_range.contains(&block.range.start)),
        );
        let offset = reparsed.offset as isize;
        for markup in &mut fragment.markup {
            markup.shift(offset);
        }
        for block in &mut fragment.blocks {
            block.shift(offset);
        }
        for (range, _) in &mut fragment.links {
            shift_range(range, offset);
        }
        for task in &mut fragment.tasks {
            task.shift(offset);
        }
        let delta = reparsed.new_range.len() as isize - reparsed.old_range.len() as isize;
        let range = &reparsed.old_range;
        splice_items(
            &mut self.markup,
            fragment.markup,
            range,
            delta,
            |item| item.range.start,
            Markup::shift,
        );
        splice_items(
            &mut self.blocks,
            fragment.blocks,
            range,
            delta,
            |item| item.range.start,
            Block::shift,
        );
        splice_items(
            &mut self.links,
            fragment.links,
            range,
            delta,
            |item| item.0.start,
            |item, delta| shift_range(&mut item.0, delta),
        );
        splice_items(
            &mut self.tasks,
            fragment.tasks,
            range,
            delta,
            |item| item.start,
            Task::shift,
        );
        self.blocks.sort_by_key(|block| block.range.start);
        self.tasks.sort_by_key(|task| task.start);
        self.fenced_code_blocks = 0;
        for block in &mut self.blocks {
            if block.code_fence {
                if let Some(code) = &mut block.code {
                    code.ordinal = self.fenced_code_blocks;
                }
                self.fenced_code_blocks += 1;
            }
        }
    }

    #[cfg(test)]
    fn parse(source: &str) -> Self {
        Self::parse_indexed(source).0
    }

    #[cfg(test)]
    fn parse_indexed(source: &str) -> (Self, MarkdownIndex) {
        let Ok(root) = crate::text::wiki::parse(source, &crate::text::advanced::parse_options())
        else {
            return (Self::default(), MarkdownIndex::default());
        };
        let index = MarkdownIndex::new(source, &root);
        (Self::from_root(source, &root), index)
    }

    fn from_root(source: &str, root: &Node) -> Self {
        let mut document = Self::default();
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
        // Lists stay source lines, so a click reveals just the line under the
        // caret. Quotes and footnotes render as blocks, tasks included.
        let rendered_block = matches!(
            node,
            Node::Heading(_)
                | Node::Code(_)
                | Node::Table(_)
                | Node::Blockquote(_)
                | Node::ThematicBreak(_)
                | Node::FootnoteDefinition(_)
                | Node::Math(_)
        ) || matches!(node, Node::Paragraph(_))
            && node.children().is_some_and(|children| {
                children
                    .iter()
                    .any(|child| matches!(child, Node::Image(_) | Node::ImageReference(_)))
            })
            || matches!(node, Node::Paragraph(_) | Node::ListItem(_))
                && crate::text::advanced::contains_inline_math(node);
        if rendered_block {
            let mut tasks = Vec::new();
            collect_tasks(node, source, &mut tasks);
            // While a block's source is revealed, its inactive task lines keep
            // their markers as checkbox glyphs.
            for task in &tasks {
                self.task_glyph(task);
            }
            let code = if let Node::Code(code) = node
                && !crate::text::advanced::is_advanced_fence(code.lang.as_deref())
            {
                self.fenced_code_blocks += 1;
                code_editor::fenced_code(
                    source,
                    &range,
                    code.lang.as_deref(),
                    &code.value,
                    self.fenced_code_blocks - 1,
                )
            } else {
                None
            };
            self.blocks.push(Block {
                id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
                range: line_range(source, &range),
                source_start: range.start,
                source: format!("{}{}", &source[range], self.definitions).into(),
                tasks,
                code,
                code_fence: matches!(node, Node::Code(code) if !crate::text::advanced::is_advanced_fence(code.lang.as_deref())),
                cache: Rc::default(),
            });
            return;
        }

        if let Some(task) = Task::of(node, source) {
            self.task_glyph(&task);
            self.tasks.push(task);
        } else if let Node::ListItem(item) = node {
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
            Node::Link(link) => {
                self.links.push((range.clone(), link.url.clone().into()));
                // The synthetic label copies literal source; do not apply the
                // ordinary text-node highlight/escape rules inside a wiki name.
                if crate::text::wiki::WikiLink::parse(&source[range.clone()]).is_some() {
                    return;
                }
            }
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

    /// Show a task marker as a checkbox (Zeron's composer), except while its
    /// line is active. The text keeps room that [`task_checkboxes`] paints.
    fn task_glyph(&mut self, task: &Task) {
        self.markup.push(Markup {
            range: task.line.clone(),
            mark: None,
            replacements: vec![replacement(task.glyph.clone(), TASK_PLACEHOLDER)],
        });
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

/// Sendable parser output. UI caches and nested editors stay on the UI thread.
enum Analysis {
    Partial(ReparsedMarkdown),
    Full(String, Option<Node>),
}

fn rope_edit(old: &super::Rope, new: &super::Rope) -> MarkdownEdit {
    let start = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = old
        .slice(start..)
        .chars_at(old.len() - start)
        .reversed()
        .zip(new.slice(start..).chars_at(new.len() - start).reversed())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    MarkdownEdit {
        range: start..old.len() - suffix,
        new_len: new.len() - start - suffix,
    }
}

impl MarkdownDisplay {
    fn new(state: WeakEntity<EditorState>) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|weak_self| {
            RefCell::new(MarkdownDisplay {
                state,
                text: super::Rope::new(),
                document: MarkdownDocument::default(),
                index: MarkdownIndex::default(),
                parsed_document: MarkdownDocument::default(),
                parsed_text: super::Rope::new(),
                weak_self: weak_self.clone(),
                parse_task: None,
                revision: 0,
                ready: None,
                #[cfg(test)]
                parse_gate: None,
                #[cfg(test)]
                parse_counts: (0, 0, 0),
                pending_edit: None,
                pending_text: None,
                last_display: None,
                code_focus: Rc::default(),
                task_glyphs: Vec::new(),
                image_root: None,
                notes: None,
            })
        })
    }

    fn accept(&mut self, text: super::Rope, index: MarkdownIndex, analysis: Analysis) {
        let mut document = match analysis {
            Analysis::Partial(reparsed) => {
                let mut document = self.parsed_document.clone();
                document.splice(reparsed);
                document
            }
            Analysis::Full(source, root) => root
                .as_ref()
                .map(|root| MarkdownDocument::from_root(&source, root))
                .unwrap_or_default(),
        };
        document.reuse_blocks(self.document.blocks.iter());
        // Content edits keep the nested editor mounted, including focus/IME.
        let code_ids: HashMap<_, _> = self
            .document
            .blocks
            .iter()
            .filter(|block| block.code.is_some())
            .map(|block| ((block.range.start, block.range.end), block.id))
            .collect();
        for block in &mut document.blocks {
            if block.code.is_some()
                && let Some(id) = code_ids.get(&(block.range.start, block.range.end))
            {
                block.id = *id;
            }
        }
        self.document = document;
        self.parsed_document = self.document.clone();
        self.index = index;
        self.parsed_text = text;
        self.pending_edit = None;
        self.pending_text = None;
        self.last_display = None;
    }
}

struct MarkdownDisplay {
    state: WeakEntity<EditorState>,
    text: super::Rope,
    document: MarkdownDocument,
    index: MarkdownIndex,
    parsed_document: MarkdownDocument,
    parsed_text: super::Rope,
    weak_self: Weak<RefCell<Self>>,
    parse_task: Option<gpui::Task<()>>,
    revision: u64,
    ready: Option<(u64, super::Rope, MarkdownIndex, Analysis)>,
    #[cfg(test)]
    parse_gate: Option<crate::async_util::Receiver<()>>,
    #[cfg(test)]
    parse_counts: (usize, usize, usize),
    pending_edit: Option<MarkdownEdit>,
    pending_text: Option<super::Rope>,
    /// Keep the projection stable during mouse selection, including mouse down.
    last_display: Option<EditorDisplay>,
    code_focus: Rc<RefCell<code_editor::CodeFocus>>,
    /// Tasks whose checkbox glyph is currently shown.
    task_glyphs: Vec<Task>,
    image_root: Option<PathBuf>,
    notes: Option<MarkdownNotes>,
}

fn display_state(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> Rc<RefCell<MarkdownDisplay>> {
    let weak = state.downgrade();
    window
        .use_keyed_state(("markdown-display", state.entity_id()), cx, move |_, _| {
            MarkdownDisplay::new(weak)
        })
        .read(cx)
        .clone()
}

/// Keep presentation caches and visited code editors alive through mode changes.
pub(super) fn retain_session(state: &Entity<EditorState>, window: &mut Window, cx: &mut App) {
    let _ = display_state(state, window, cx);
}

pub(super) fn provider(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> SharedEditorDisplayProvider {
    display_state(state, window, cx)
}

pub(super) fn set_image_root(
    state: &Entity<EditorState>,
    image_root: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let display = display_state(state, window, cx);
    let mut display = display.borrow_mut();
    if display.image_root != image_root {
        display.image_root = image_root;
        display.last_display = None;
        for block in &mut display.document.blocks {
            block.cache = Rc::default();
        }
        state.update(cx, |_, cx| cx.notify());
    }
}

pub(super) fn set_notes(
    state: &Entity<EditorState>,
    notes: MarkdownNotes,
    window: &mut Window,
    cx: &mut App,
) {
    let display = display_state(state, window, cx);
    let mut display = display.borrow_mut();
    if display.notes.as_ref() != Some(&notes) {
        display.notes = Some(notes);
        display.last_display = None;
        for block in &mut display.document.blocks {
            block.cache = Rc::default();
        }
        state.update(cx, |_, cx| cx.notify());
    }
}

/// Paint the checkboxes of tasks whose marker is hidden, over the room
/// their line reserves.
pub(super) fn task_checkboxes(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let display = display_state(state, window, cx);
    let state = state.clone();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let tasks = display.borrow().task_glyphs.clone();
            let theme = cx.theme();
            let (primary, foreground, border) =
                (theme.primary, theme.primary_foreground, theme.border);
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                for task in tasks {
                    let Some(room) = state.read(cx).range_to_bounds(&task.glyph) else {
                        continue;
                    };
                    let side = px(TASK_CHECKBOX_SIZE).min(room.size.height - px(4.));
                    let checkbox = Bounds::new(
                        point(room.origin.x + px(1.), room.center().y - side / 2.),
                        size(side, side),
                    );
                    window.paint_quad(quad(
                        checkbox,
                        px(3.),
                        if task.checked {
                            primary
                        } else {
                            gpui::transparent_black()
                        },
                        px(1.),
                        if task.checked { primary } else { border },
                        BorderStyle::Solid,
                    ));
                    if task.checked {
                        let inset = side * 0.15;
                        _ = window.paint_svg(
                            Bounds::new(
                                checkbox.origin + point(inset, inset),
                                size(side - inset * 2., side - inset * 2.),
                            ),
                            IconName::Check.path(),
                            None,
                            TransformationMatrix::unit(),
                            foreground,
                            cx,
                        );
                    }
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Toggle a task when its checkbox glyph is clicked, before the editor moves
/// its caret there and reveals the line's source.
pub(super) fn task_mouse_down(
    state: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    let display = display_state(state, window, cx);
    let state = state.downgrade();
    move |event, window, cx| {
        if event.button != MouseButton::Left || event.modifiers.modified() {
            return;
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        let task = display.borrow().task_glyphs.iter().find_map(|task| {
            state
                .read(cx)
                .range_to_bounds(&task.glyph)
                .filter(|bounds| bounds.dilate(px(2.)).contains(&event.position))
                .map(|_| task.clone())
        });
        if let Some(task) = task {
            cx.stop_propagation();
            set_task(&state, &task.marker, !task.checked, window, cx);
        }
    }
}

impl EditorDisplayProvider for MarkdownDisplay {
    fn text_changed(
        &mut self,
        old_text: &super::Rope,
        text: &super::Rope,
        range: &Range<usize>,
        new_len: usize,
    ) {
        let previous = self.pending_text.as_ref().unwrap_or(&self.text);
        if !ropey::extra::esoterica::ropes_are_instances(previous, old_text) {
            self.pending_edit = None;
        } else if let Some(edit) = &mut self.pending_edit {
            // Map the next edit back into the parsed snapshot. Positions inside
            // the previous replacement belong to its entire old source range.
            let new_end = edit.range.start + edit.new_len;
            let delta = edit.new_len as isize - edit.range.len() as isize;
            let start = if range.start < edit.range.start {
                range.start
            } else if range.start <= new_end {
                edit.range.start
            } else {
                range.start.checked_add_signed(-delta).unwrap()
            };
            let end = if range.end < edit.range.start {
                range.end
            } else if range.end <= new_end {
                edit.range.end
            } else {
                range.end.checked_add_signed(-delta).unwrap()
            };
            let combined = start.min(edit.range.start)..end.max(edit.range.end);
            edit.new_len = combined
                .len()
                .checked_add_signed(delta + new_len as isize - range.len() as isize)
                .unwrap();
            edit.range = combined;
        } else if self.pending_text.is_none() {
            self.pending_edit = Some(MarkdownEdit {
                range: range.clone(),
                new_len,
            });
        }
        if ropey::extra::esoterica::ropes_are_instances(&self.text, old_text) {
            self.document.project_edit(text, range, new_len);
            self.text = text.clone();
            self.last_display = None;
        }
        self.revision = self.revision.wrapping_add(1);
        self.pending_text = Some(text.clone());
    }
    fn prepare(&mut self, text: &super::Rope, cx: &mut App) {
        if !ropey::extra::esoterica::ropes_are_instances(&self.text, text) {
            // Source mode and external replacements can bypass provider events.
            // Compare Rope characters without copying the whole document on UI.
            let edit = rope_edit(&self.text, text);
            let old = self.text.clone();
            self.text_changed(&old, text, &edit.range, edit.new_len);
        }
        if let Some((revision, snapshot, index, analysis)) = self.ready.take() {
            if revision == self.revision
                && ropey::extra::esoterica::ropes_are_instances(&snapshot, text)
            {
                if self
                    .state
                    .upgrade()
                    .is_some_and(|state| state.read(cx).is_selecting())
                {
                    self.ready = Some((revision, snapshot, index, analysis));
                    return;
                }
                self.accept(snapshot, index, analysis);
            }
        }
        if ropey::extra::esoterica::ropes_are_instances(&self.parsed_text, text)
            || self.parse_task.is_some()
        {
            return;
        }
        let weak = self.weak_self.clone();
        self.parse_task = Some(cx.spawn(async move |cx| {
            loop {
                let request = weak.upgrade().and_then(|display| {
                    let display = display.borrow();
                    (!ropey::extra::esoterica::ropes_are_instances(
                        &display.parsed_text,
                        &display.text,
                    ))
                    .then(|| {
                        (
                            display.revision,
                            display.text.clone(),
                            display.index.clone(),
                            display.pending_edit.clone(),
                        )
                    })
                });
                let Some((revision, text, mut index, edit)) = request else {
                    break;
                };
                #[cfg(test)]
                {
                    let gate = weak.upgrade().and_then(|display| {
                        let mut display = display.borrow_mut();
                        display.parse_counts.0 += 1;
                        display.parse_gate.take()
                    });
                    if let Some(gate) = gate {
                        let _ = gate.recv().await;
                    }
                }
                let snapshot = text.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let options = crate::text::advanced::parse_options();
                        if let Some(reparsed) = edit.and_then(|edit| {
                            index.reparse(
                                snapshot.len(),
                                &edit,
                                |range| snapshot.slice(range).to_string(),
                                &options,
                            )
                        }) {
                            return (index, Analysis::Partial(reparsed));
                        }
                        let source = snapshot.to_string();
                        let root = crate::text::wiki::parse(&source, &options).ok();
                        let index = root
                            .as_ref()
                            .map(|root| MarkdownIndex::new(&source, root))
                            .unwrap_or_default();
                        (index, Analysis::Full(source, root))
                    })
                    .await;
                let keep_running = cx.update(|cx| {
                    let Some(display) = weak.upgrade() else {
                        return false;
                    };
                    let mut display = display.borrow_mut();
                    let Some(state) = display.state.upgrade() else {
                        return false;
                    };
                    // Check both the provider generation and the authoritative
                    // editor Rope: the editor may change before the next frame.
                    if display.revision == revision
                        && ropey::extra::esoterica::ropes_are_instances(
                            state.read(cx).text(),
                            &text,
                        )
                        && ropey::extra::esoterica::ropes_are_instances(&display.text, &text)
                    {
                        #[cfg(test)]
                        {
                            display.parse_counts.1 += 1;
                        }
                        if state.read(cx).is_selecting() {
                            display.ready = Some((revision, text, result.0, result.1));
                        } else {
                            display.accept(text, result.0, result.1);
                        }
                        state.update(cx, |_, cx| cx.notify());
                    } else {
                        #[cfg(test)]
                        {
                            display.parse_counts.2 += 1;
                        }
                    }
                    display.revision != revision
                });
                if !keep_running {
                    break;
                }
            }
            if let Some(display) = weak.upgrade() {
                display.borrow_mut().parse_task = None;
            }
        }));
    }
    fn link_handler(&self) -> Option<Rc<dyn Fn(&SharedString, &mut Window, &mut App)>> {
        let notes = self.notes.clone();
        Some(Rc::new(move |url, window, cx| {
            if let Some(target) = crate::text::wiki::note_target(url) {
                if let Some(notes) = &notes {
                    notes.navigate(target, window, cx);
                }
            } else {
                cx.open_url(url);
            }
        }))
    }
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
        window: &Window,
        cx: &App,
    ) -> EditorDisplay {
        #[cfg(test)]
        {
            tests::LAST_CODE_FOCUS.with(|last| *last.borrow_mut() = Some(self.code_focus.clone()));
            tests::LAST_DISPLAY.with(|last| *last.borrow_mut() = self.weak_self.clone());
        }
        let unchanged = ropey::extra::esoterica::ropes_are_instances(&self.text, text);
        if unchanged
            && self
                .state
                .upgrade()
                .is_some_and(|state| state.read(cx).is_selecting())
            && let Some(display) = &self.last_display
        {
            // The selection highlight still follows the real source range, but
            // revealing syntax here would move the text beneath the pointer.
            return display.clone();
        }
        let active = focused || !selection.is_empty();
        self.task_glyphs = self
            .document
            .tasks
            .iter()
            .filter(|task| !active || !touches(&selection, &task.line))
            .cloned()
            .collect();
        let mut display = EditorDisplay {
            replacements: self.document.replacements(&selection, focused),
            ..Default::default()
        };
        let urls: HashMap<_, _> = self
            .document
            .links
            .iter()
            .map(|(range, url)| (range.start, url))
            .collect();
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
                    color: Some(
                        if urls
                            .get(&markup.range.start)
                            .and_then(|url| crate::text::wiki::note_target(url))
                            .is_some_and(|target| {
                                !self
                                    .notes
                                    .as_ref()
                                    .is_some_and(|notes| notes.resolve(target).is_some())
                            })
                        {
                            cx.theme().danger
                        } else {
                            cx.theme().primary
                        },
                    ),
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
        self.code_focus.borrow_mut().retain_editors(
            &self
                .document
                .blocks
                .iter()
                .filter(|block| block.code.is_some())
                .map(|block| block.id)
                .collect(),
        );
        for block in &self.document.blocks {
            if let Some(code) = &block.code {
                let inside = touches(&selection, &block.range);
                if self
                    .code_focus
                    .borrow_mut()
                    .shows_source(code, &selection, inside, focused)
                {
                    continue;
                }
                let editable = Rc::new(code_editor::EditableCode {
                    document: self.state.clone(),
                    document_id: self.state.entity_id(),
                    id: block.id,
                    code: code.clone(),
                    lines: block.range.clone(),
                    focus: self.code_focus.clone(),
                });
                let height = editable.height_hint(window, cx);
                block.cache.borrow_mut().set_height_hint(move |_| height);
                display.blocks.push(EditorDisplayBlock {
                    range: block.range.clone(),
                    cache: block.cache.clone(),
                    render: Rc::new(move |window, cx| editable.render(window, cx)),
                });
                continue;
            }
            if (focused || !selection.is_empty()) && touches(&selection, &block.range) {
                continue;
            }
            let source = block.source.clone();
            let image_root = self.image_root.clone();
            let notes = self.notes.clone();
            let state = self.state.clone();
            let range = block.range.clone();
            let tasks = block.tasks.clone();
            let source_start = block.source_start;
            let id = format!("markdown-block-{:?}-{}", self.state.entity_id(), block.id);
            display.blocks.push(EditorDisplayBlock {
                range: range.clone(),
                cache: block.cache.clone(),
                render: Rc::new(move |_, cx| {
                    let task_state = state.clone();
                    let tasks = tasks.clone();
                    let view = TextView::markdown(SharedString::from(id.clone()), source.clone())
                        .style(markdown_style(cx, &image_root, &notes).paragraph_gap(rems(0.25)))
                        .markdown_extensions(code_block::extensions())
                        .on_task_toggle(move |offset, checked, window, cx| {
                            if let Some(task) = tasks
                                .iter()
                                .find(|task| task.start == source_start + offset)
                                && let Some(state) = task_state.upgrade()
                            {
                                set_task(&state, &task.marker, checked, window, cx);
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
                            if event.modifiers.secondary() {
                                return;
                            }
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
        self.last_display = Some(display.clone());
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext, Context, Render, TestAppContext, VisualTestContext, point,
        prelude::FluentBuilder as _,
    };

    thread_local! {
        pub(super) static LAST_DISPLAY: RefCell<Weak<RefCell<MarkdownDisplay>>> = const { RefCell::new(Weak::new()) };
        pub(super) static LAST_CODE_FOCUS: RefCell<Option<Rc<RefCell<code_editor::CodeFocus>>>> = const { RefCell::new(None) };
    }

    pub(super) struct MarkdownEditorTest {
        state: Entity<EditorState>,
        mode: MarkdownMode,
        readonly: bool,
        content_padding: Option<Edges<Pixels>>,
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
                        .when_some(self.content_padding, |editor, padding| {
                            editor.content_padding(padding)
                        })
                        .size_full(),
                )
        }
    }

    pub(super) fn editor<'a>(
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
                content_padding: None,
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

    fn assert_current_analysis(display: &MarkdownDisplay, source: &str) {
        assert_eq!(display.parsed_text.to_string(), source);
        let expected = MarkdownDocument::parse(source);
        assert_eq!(display.document.links, expected.links);
        assert_eq!(
            display
                .document
                .replacements(&(source.len()..source.len()), false),
            expected.replacements(&(source.len()..source.len()), false)
        );
        assert_eq!(display.document.blocks.len(), expected.blocks.len());
        for (actual, expected) in display.document.blocks.iter().zip(&expected.blocks) {
            assert_eq!(actual.range, expected.range);
            assert_eq!(actual.source, expected.source);
            assert_eq!(
                actual.code.as_ref().map(|code| (&code.content, &code.code)),
                expected
                    .code
                    .as_ref()
                    .map(|code| (&code.content, &code.code))
            );
        }
        assert_eq!(
            display
                .document
                .tasks
                .iter()
                .map(|task| (&task.marker, task.checked))
                .collect::<Vec<_>>(),
            expected
                .tasks
                .iter()
                .map(|task| (&task.marker, task.checked))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rope_edit_keeps_utf8_boundaries_and_minimal_replacement() {
        for (old, new) in [
            ("niño 世界 &amp;", "niña 世界 &amp;"),
            ("", "中"),
            ("é中", ""),
            ("same", "same"),
            ("**abc** suffix", "**a中bc** suffix"),
        ] {
            let old_rope = super::super::Rope::from(old);
            let new_rope = super::super::Rope::from(new);
            let edit = rope_edit(&old_rope, &new_rope);
            let mut reconstructed = old.to_string();
            reconstructed.replace_range(
                edit.range.clone(),
                &new[edit.range.start..edit.range.start + edit.new_len],
            );
            assert_eq!(reconstructed, new);
        }
    }

    #[test]
    fn provisional_projection_shifts_cached_blocks_and_invalidates_dirty_tokens() {
        let source =
            "**niño** &amp; suffix\n\n| Header |\n| --- |\n| cell |\n\n```rust\nlet x = 1;\n```";
        let mut document = MarkdownDocument::parse(source);
        let blocks = document
            .blocks
            .iter()
            .map(|block| (block.id, block.range.clone(), block.cache.clone()))
            .collect::<Vec<_>>();
        let offset = source.find("niño").unwrap();
        let mut updated = source.to_string();
        updated.insert_str(offset, "中");
        document.project_edit(
            &super::super::Rope::from(updated.as_str()),
            &(offset..offset),
            "中".len(),
        );
        assert!(!document.markup.iter().any(|markup| markup.range.start == 0));
        for (block, (id, old_range, cache)) in document.blocks.iter().zip(blocks) {
            assert_eq!(block.id, id);
            assert_eq!(block.range, old_range.start + 3..old_range.end + 3);
            assert!(Rc::ptr_eq(&block.cache, &cache));
        }
        let code = document
            .blocks
            .iter()
            .find(|block| block.code.is_some())
            .unwrap();
        let id = code.id;
        let offset = code.code.as_ref().unwrap().content.start;
        updated.insert_str(offset, "// é\n");
        document.project_edit(
            &super::super::Rope::from(updated.as_str()),
            &(offset..offset),
            "// é\n".len(),
        );
        let code = document
            .blocks
            .iter()
            .find(|block| block.id == id)
            .unwrap()
            .code
            .as_ref()
            .unwrap();
        assert!(code.code.starts_with("// é\n"));
        assert_eq!(&updated[code.content.clone()], "// é\nlet x = 1;\n");
    }

    #[gpui::test]
    fn background_analysis_coalesces_rapid_edits_and_rejects_stale_results(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let source = (0..64).map(|i| format!("## Section {i}\n\n| Col | Value |\n| --- | --- |\n| niño | 世界 &amp; |\n\n```typescript\nconst v{i} = {i};\n```\n\n[link][ref]\n\n")).collect::<String>() + "[ref]: https://example.com\n\nTail";
        let (_, state, cx) = editor(cx, &source);
        redraw(cx);
        let display = LAST_DISPLAY.with(|last| last.borrow().upgrade().unwrap());
        assert_current_analysis(&display.borrow(), &source);
        let counts = display.borrow().parse_counts;
        let ids = display
            .borrow()
            .document
            .blocks
            .iter()
            .map(|block| block.id)
            .collect::<Vec<_>>();
        let (tx, rx) = crate::async_util::unbounded();
        display.borrow_mut().parse_gate = Some(rx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
                state.replace_text_in_range(None, "a", window, cx);
            })
        });
        redraw(cx); // The first snapshot is now held at the parser gate.
        let mut expected = source.clone() + "a";
        for character in "niño 世界 **bold** &amp;".chars() {
            let text = character.to_string();
            cx.update(|window, cx| {
                state.update(cx, |state, cx| {
                    state.replace_text_in_range(None, &text, window, cx)
                });
                window.draw(cx).clear();
            });
            expected.push(character);
        }
        {
            let display = display.borrow();
            assert_eq!(display.parse_counts, (counts.0 + 1, counts.1, counts.2));
            assert_eq!(
                display.parsed_text.to_string(),
                source,
                "no synchronous parser on input/layout"
            );
            assert_eq!(display.text.to_string(), expected);
            assert_eq!(
                display
                    .document
                    .blocks
                    .iter()
                    .map(|block| block.id)
                    .collect::<Vec<_>>(),
                ids
            );
        }
        tx.try_send(()).unwrap();
        redraw(cx);
        redraw(cx);
        assert_current_analysis(&display.borrow(), &expected);
        assert_eq!(
            display.borrow().parse_counts,
            (counts.0 + 2, counts.1 + 1, counts.2 + 1)
        );
    }

    #[gpui::test]
    fn stale_analysis_cannot_replace_a_new_source_mode_document(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "# Old\n\n| H |\n| --- |\n| é |\n\n```rust\nlet x = 1;\n```\n\n[ref]: https://old.example\n\nTail";
        let (view, state, cx) = editor(cx, source);
        redraw(cx);
        let display = LAST_DISPLAY.with(|last| last.borrow().upgrade().unwrap());
        let (tx, rx) = crate::async_util::unbounded();
        display.borrow_mut().parse_gate = Some(rx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
                state.replace_text_in_range(None, " pending", window, cx);
            })
        });
        redraw(cx);
        view.update(cx, |view, cx| {
            view.mode = MarkdownMode::Source;
            cx.notify();
        });
        redraw(cx);
        let new_source =
            "# New 世界\n\n[link][ref]\n\n[ref]: https://new.example\n\n```rust\nnew();\n```";
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.set_value(new_source, window, cx))
        });
        tx.try_send(()).unwrap();
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            new_source
        );
        assert_ne!(
            display.borrow().parsed_text.to_string(),
            format!("{source} pending")
        );
        for mode in [MarkdownMode::Preview, MarkdownMode::LivePreview] {
            view.update(cx, |view, cx| {
                view.mode = mode;
                cx.notify();
            });
            redraw(cx);
            redraw(cx);
        }
        assert_current_analysis(&display.borrow(), new_source);
        assert!(
            display
                .borrow()
                .document
                .links
                .iter()
                .any(|(_, url)| url.as_ref() == "https://new.example")
        );
        assert!(display.borrow().parse_counts.2 >= 1);
    }

    #[gpui::test]
    fn background_analysis_preserves_unicode_composition_and_history(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "| H |\n| --- |\n| é |\n\n[ref]: https://example.com\n\nTail ";
        let (_, state, cx) = editor(cx, source);
        redraw(cx);
        let display = LAST_DISPLAY.with(|last| last.borrow().upgrade().unwrap());
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
                state.replace_and_mark_text_in_range(None, "中", Some(1..1), window, cx);
            })
        });
        redraw(cx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                assert!(state.marked_text_range(window, cx).is_some());
                state.replace_and_mark_text_in_range(None, "世界", Some(2..2), window, cx);
            })
        });
        redraw(cx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                assert!(state.marked_text_range(window, cx).is_some());
                state.replace_text_in_range(None, "世界", window, cx);
            })
        });
        redraw(cx);
        assert_current_analysis(&display.borrow(), &format!("{source}世界"));
        cx.dispatch_action(super::super::Undo);
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            source
        );
        cx.dispatch_action(super::super::Redo);
        redraw(cx);
        assert_current_analysis(&display.borrow(), &format!("{source}世界"));
    }

    #[gpui::test]
    fn partial_background_edits_match_full_analysis_for_tables_and_fences(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "intro **bold**\n\n| Header | Value |\n| --- | --- |\n| niño | 世界 |\n\n```rust\nlet x = 1;\n```\n\nend";
        let (_, state, cx) = editor(cx, source);
        redraw(cx);
        let display = LAST_DISPLAY.with(|last| last.borrow().upgrade().unwrap());
        for (needle, replacement) in [
            ("niño", "é中"),
            ("--- |", "---: |"),
            ("let x = 1;", "// é\nlet x = 2;"),
            ("```\n\nend", "\n\nend"),
            ("**bold**", "_new_"),
        ] {
            cx.update(|window, cx| {
                state.update(cx, |state, cx| state.set_value(source, window, cx))
            });
            redraw(cx);
            let start = source.find(needle).unwrap();
            let mut expected = source.to_string();
            expected.replace_range(start..start + needle.len(), replacement);
            cx.update(|window, cx| {
                state.update(cx, |state, cx| {
                    state.set_selected_range(start..start + needle.len(), cx);
                    state.focus(window, cx);
                    state.replace_text_in_range(None, replacement, window, cx);
                });
                assert_eq!(display.borrow().parsed_text.to_string(), source);
                assert_eq!(
                    state.read(cx).selected_range(),
                    start + replacement.len()..start + replacement.len()
                );
            });
            redraw(cx);
            redraw(cx);
            assert_current_analysis(&display.borrow(), &expected);
        }
    }

    #[gpui::test]
    fn retired_presentation_does_not_keep_its_parser_job_alive(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, state, cx) = editor(cx, "**niño**\n\n[ref]: https://example.com");
        let display = MarkdownDisplay::new(state.downgrade());
        let weak = Rc::downgrade(&display);
        let (tx, rx) = crate::async_util::unbounded();
        display.borrow_mut().parse_gate = Some(rx);
        cx.update(|_, cx| {
            let text = state.read(cx).text().clone();
            display.borrow_mut().prepare(&text, cx);
        });
        cx.run_until_parked();
        assert_eq!(display.borrow().parse_counts.0, 1);
        drop(display);
        assert!(weak.upgrade().is_none());
        let _ = tx.try_send(());
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "**niño**\n\n[ref]: https://example.com"
        );
    }

    #[gpui::test]
    fn background_publication_waits_for_drag_release(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "**bold** text\n\n[ref]: https://example.com\n\nTail";
        let (_, state, cx) = editor(cx, source);
        redraw(cx);
        let display = LAST_DISPLAY.with(|last| last.borrow().upgrade().unwrap());
        let (tx, rx) = crate::async_util::unbounded();
        display.borrow_mut().parse_gate = Some(rx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
                state.replace_text_in_range(None, " é", window, cx);
            })
        });
        redraw(cx);
        let bounds = state
            .read_with(cx, |state, _| state.range_to_bounds(&(3..4)))
            .unwrap();
        cx.simulate_mouse_down(
            bounds.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        redraw(cx);
        let replacements = display
            .borrow()
            .last_display
            .as_ref()
            .unwrap()
            .replacements
            .clone();
        tx.try_send(()).unwrap();
        redraw(cx);
        assert!(display.borrow().ready.is_some());
        assert_eq!(display.borrow().parsed_text.to_string(), source);
        assert_eq!(
            display.borrow().last_display.as_ref().unwrap().replacements,
            replacements
        );
        cx.simulate_mouse_up(
            bounds.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        redraw(cx);
        assert!(display.borrow().ready.is_none());
        assert_current_analysis(&display.borrow(), &format!("{source} é"));
    }

    #[gpui::test]
    fn many_fences_only_create_visible_editors_and_retain_them_across_edits(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let source = (0..64)
            .map(|index| format!("```typescript\nconst item{index} = {index};\n```\n\n"))
            .collect::<String>();
        let (view, state, cx) = editor(cx, &source);
        let focus = LAST_CODE_FOCUS.with(|last| last.borrow().clone()).unwrap();
        let initial = focus.borrow().editor_ids();
        assert!(!initial.is_empty());
        assert!(
            initial.len() < 16,
            "cold layout must not construct all 64 editors: {}",
            initial.len()
        );
        // Edit the visible first fence through its nested editor.
        let nested = cx.read(|cx| focus.borrow().first_editor(cx).unwrap());
        cx.update(|window, cx| {
            nested.update(cx, |nested, cx| {
                nested.focus(window, cx);
                nested.set_selected_range(0..0, cx);
                nested.replace_text_in_range(None, "// edit\n", window, cx);
            })
        });
        redraw(cx);
        assert!(
            state
                .read_with(cx, |state, _| state.value())
                .contains("// edit")
        );
        let after = focus.borrow().editor_ids();
        assert_eq!(
            initial[0], after[0],
            "editing code must preserve the nested entity"
        );
        for mode in [
            MarkdownMode::Source,
            MarkdownMode::Preview,
            MarkdownMode::LivePreview,
        ] {
            view.update(cx, |view, cx| {
                view.mode = mode;
                cx.notify();
            });
            redraw(cx);
        }
        let resumed = LAST_CODE_FOCUS.with(|last| last.borrow().clone()).unwrap();
        assert!(
            Rc::ptr_eq(&focus, &resumed),
            "Source and Preview must retain the presentation session"
        );
        assert_eq!(initial[0], resumed.borrow().editor_ids()[0]);
        view.update(cx, |view, cx| {
            view.mode = MarkdownMode::Source;
            cx.notify();
        });
        cx.update(|window, cx| state.update(cx, |state, cx| state.focus(window, cx)));
        redraw(cx);
        cx.dispatch_action(super::super::Undo);
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            source
        );
        cx.dispatch_action(super::super::Redo);
        redraw(cx);
        assert!(
            state
                .read_with(cx, |state, _| state.value())
                .contains("// edit")
        );
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

    /// Every task in `source`, with the start of its rendered block, if any.
    fn task_controls(source: &str) -> Vec<(Task, Option<usize>)> {
        let document = MarkdownDocument::parse(source);
        document
            .tasks
            .iter()
            .map(|task| (task.clone(), None))
            .chain(document.blocks.iter().flat_map(|block| {
                block
                    .tasks
                    .iter()
                    .map(|task| (task.clone(), Some(block.source_start)))
            }))
            .collect()
    }

    /// Where to click a task: its rendered checkbox inside a block, or its
    /// checkbox glyph on a source line.
    fn task_control(
        cx: &mut VisualTestContext,
        state: &Entity<EditorState>,
        task: &Task,
        block_start: Option<usize>,
    ) -> Option<Bounds<Pixels>> {
        match block_start {
            Some(block_start) => {
                // GPUI's test lookup requires a static selector.
                let selector = Box::leak(
                    format!("markdown-task-{}", task.start - block_start).into_boxed_str(),
                );
                cx.debug_bounds(selector)
            }
            None => state.read_with(cx, |state, _| state.range_to_bounds(&task.glyph)),
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
            let tasks = task_controls(source);
            let (content, state, cx) = editor(cx, source);
            let mut expected = source.to_owned();
            for (task, block_start) in &tasks {
                cx.update(|window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_selected_range(source.len()..source.len(), cx);
                        state.focus(window, cx);
                    })
                });
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear());
                let checkbox = task_control(cx, &state, task, *block_start)
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
            for (task, block_start) in &tasks {
                let checkbox = task_control(cx, &state, task, *block_start).unwrap();
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
    fn nested_tasks_keep_their_source_markers() {
        for (source, blocks) in [
            ("- [ ] Parent\n  - [x] Child", 0),
            ("> - [ ] Parent\n>   - [X] Child", 1),
        ] {
            assert_eq!(MarkdownDocument::parse(source).blocks.len(), blocks);
            let tasks = task_controls(source);
            assert_eq!(tasks.len(), 2);
            for (task, _) in &tasks {
                assert!(matches!(&source[task.marker.clone()], " " | "x" | "X"));
                assert!(matches!(
                    &source[task.glyph.clone()],
                    "- [ ]" | "- [x]" | "- [X]"
                ));
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
            let content = cx.new(|_| MarkdownEditorTest { state, mode: MarkdownMode::LivePreview, readonly: false, content_padding: None });
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

    const CODE_SOURCE: &str = "intro\n\n```rust\nlet x = 1;\n```\n\nend";
    const CODE_START: usize = 7;

    #[gpui::test]
    fn content_padding_insets_text_while_the_scroll_area_keeps_the_frame(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = (0..80)
            .map(|line| format!("Paragraph {line} long enough to wrap inside a narrow column.\n\n"))
            .collect::<String>();
        let (view, state, cx) = editor(cx, &source);
        let padding = Edges {
            top: px(8.),
            right: px(120.),
            bottom: px(8.),
            left: px(120.),
        };
        for mode in [MarkdownMode::LivePreview, MarkdownMode::Source] {
            view.update(cx, |view, cx| {
                view.mode = mode;
                view.content_padding = Some(padding);
                cx.notify();
            });
            redraw(cx);
            let frame = cx.debug_bounds("markdown-test-editor").unwrap();
            let text = state.read_with(cx, |state, _| state.input_bounds());
            // Inside the 1px frame border, the text column starts and ends
            // at the requested padding instead of the size preset's.
            assert_eq!(text.left(), frame.left() + px(121.), "{mode:?}");
            assert_eq!(text.right(), frame.right() - px(121.), "{mode:?}");
            // The padding still belongs to the scroll area.
            let before = state.read_with(cx, |state, _| state.scroll_offset().y);
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: point(frame.left() + px(40.), frame.top() + px(200.)),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-120.))),
                ..Default::default()
            });
            redraw(cx);
            let after = state.read_with(cx, |state, _| state.scroll_offset().y);
            assert!(after < before, "{mode:?}: {before:?} -> {after:?}");
            state.update(cx, |state, cx| {
                state.set_scroll_offset(point(px(0.), px(0.)), cx)
            });
        }
    }

    #[gpui::test]
    fn top_inset_scrolls_away_with_the_text(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = (0..80)
            .map(|line| format!("Paragraph {line} long enough to wrap inside a narrow column.\n\n"))
            .collect::<String>();
        let (view, state, cx) = editor(cx, &source);
        let inset = px(120.);
        let wheel = |cx: &mut VisualTestContext, frame: Bounds<Pixels>, dy: f32| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: point(frame.left() + px(40.), frame.top() + px(200.)),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))),
                ..Default::default()
            });
            redraw(cx);
        };
        let inset_y = |cx: &mut VisualTestContext, mode| {
            cx.update(|window, cx| super::top_inset_y(&state, mode, window, cx))
                .unwrap()
        };
        let first_row = |cx: &mut VisualTestContext| {
            state.read_with(cx, |state, _| {
                state.range_to_bounds(&(0..0)).map(|b| b.top())
            })
        };
        for mode in [
            MarkdownMode::LivePreview,
            MarkdownMode::Source,
            MarkdownMode::Preview,
        ] {
            view.update(cx, |view, cx| {
                view.mode = mode;
                view.content_padding = Some(Edges::all(px(8.)));
                cx.notify();
            });
            state.update(cx, |state, cx| state.set_top_inset(inset, cx));
            redraw(cx);
            let frame = cx.debug_bounds("markdown-test-editor").unwrap();
            // Below the top padding, inside the input's 1px frame (the
            // reading view has none).
            let border = if mode == MarkdownMode::Preview {
                px(0.)
            } else {
                px(1.)
            };
            let top = inset_y(cx, mode);
            assert_eq!(top, frame.top() + border + px(8.), "{mode:?}");
            if mode != MarkdownMode::Preview {
                assert_eq!(first_row(cx), Some(top + inset), "{mode:?}");
            }
            wheel(cx, frame, -50.);
            let scrolled = inset_y(cx, mode);
            assert_eq!(scrolled, top - px(50.), "{mode:?}");
            if mode != MarkdownMode::Preview {
                assert_eq!(first_row(cx), Some(scrolled + inset), "{mode:?}");
                // Changing the inset while scrolled keeps the text in place.
                wheel(cx, frame, -400.);
                let line = source.find("Paragraph 12").unwrap();
                let before = state.read_with(cx, |state, _| state.range_to_bounds(&(line..line)));
                state.update(cx, |state, cx| state.set_top_inset(inset + px(40.), cx));
                redraw(cx);
                let after = state.read_with(cx, |state, _| state.range_to_bounds(&(line..line)));
                assert!(before.is_some(), "{mode:?}");
                assert_eq!(before, after, "{mode:?}");
                state.update(cx, |state, cx| {
                    state.set_scroll_offset(point(px(0.), px(0.)), cx)
                });
            } else {
                wheel(cx, frame, 1000.);
            }
            state.update(cx, |state, cx| state.set_top_inset(px(0.), cx));
            redraw(cx);
            assert_eq!(inset_y(cx, mode), top, "{mode:?}");
        }
    }

    pub(super) fn redraw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn code_blocks_are_edited_in_place(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, state, cx) = editor(cx, CODE_SOURCE);
        let body = cx
            .debug_bounds("markdown-code-editor-7")
            .expect("fenced code must render an editor");
        cx.simulate_click(body.center(), gpui::Modifiers::default());
        redraw(cx);
        cx.simulate_input("Z");
        redraw(cx);
        let value = state.read_with(cx, |state, _| state.value());
        assert!(value.starts_with("intro\n\n```rust\n"), "{value:?}");
        assert!(value.ends_with("\n```\n\nend"), "{value:?}");
        assert_eq!(value.len(), CODE_SOURCE.len() + 1);
        assert!(cx.debug_bounds("markdown-live-block-7").is_some());
        assert!(cx.debug_bounds("markdown-code-editor-7").is_some());
    }

    #[gpui::test]
    fn caret_moves_through_code_blocks_and_escape_reveals_fences(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, state, cx) = editor(cx, CODE_SOURCE);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_selected_range(CODE_START..CODE_START, cx);
                state.focus(window, cx);
            })
        });
        redraw(cx);
        cx.simulate_input("Z");
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "intro\n\n```rust\nZlet x = 1;\n```\n\nend"
        );
        assert!(cx.debug_bounds("markdown-live-block-7").is_some());

        // Leaving the first code line returns to the document above the block.
        cx.dispatch_action(super::super::MoveUp);
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.selected_range()),
            CODE_START - 1..CODE_START - 1
        );
        assert!(cx.debug_bounds("markdown-live-block-7").is_some());

        // Moving down re-enters the code; Escape reveals the fences as source.
        cx.dispatch_action(super::super::MoveDown);
        redraw(cx);
        cx.dispatch_action(super::super::Escape);
        redraw(cx);
        assert!(cx.debug_bounds("markdown-live-block-7").is_none());
        let caret = state.read_with(cx, |state, _| state.selected_range());
        assert!(caret.start > CODE_START + "```rust".len(), "{caret:?}");

        // Undo in the document resynchronizes the nested editor.
        cx.dispatch_action(super::super::Undo);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_selected_range(0..0, cx);
                state.focus(window, cx);
            })
        });
        redraw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            CODE_SOURCE
        );
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        cx.simulate_click(body.center(), gpui::Modifiers::default());
        redraw(cx);
        cx.simulate_input("Y");
        redraw(cx);
        let value = state.read_with(cx, |state, _| state.value());
        assert!(!value.contains('Z') && value.contains('Y'), "{value:?}");
    }

    #[gpui::test]
    fn reading_preview_code_blocks_fit_long_lines(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = format!("```\n{}\n```", "word ".repeat(80));
        let (content, _, cx) = editor(cx, &source);
        content.update(cx, |view, cx| {
            view.mode = MarkdownMode::Preview;
            cx.notify();
        });
        redraw(cx);
        let before = cx.debug_bounds("markdown-editor-code-block-0").unwrap();
        let fit = cx
            .debug_bounds("markdown-editor-code-block-0-fit")
            .expect("the preview header must offer fit content");
        cx.simulate_click(fit.center(), gpui::Modifiers::default());
        redraw(cx);
        let after = cx.debug_bounds("markdown-editor-code-block-0").unwrap();
        assert_eq!(after.size.width, before.size.width);
        assert!(
            after.size.height > before.size.height * 2.,
            "{before:?} -> {after:?}"
        );
    }

    #[gpui::test]
    fn code_block_copy_keeps_the_rendered_block(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "```rust\nlet x = 1;\n```\n\nend";
        let (_, state, cx) = editor(cx, source);
        let copy = cx
            .debug_bounds("markdown-editor-code-block-0-copy")
            .expect("code block header must have a copy action");
        cx.simulate_click(copy.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("let x = 1;")
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            source
        );
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("markdown-live-block-0").is_some());
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
                content_padding: None,
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
        let end = "- [ ] Task\n\nend".len();
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let checkbox = state
            .read_with(cx, |state, _| state.range_to_bounds(&(0..5)))
            .expect("task glyph must be laid out");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "- [x] Task\n\nend"
        );
        // Toggling neither moves the caret nor reveals the task's source.
        assert_eq!(
            state.read_with(cx, |state, _| state.selected_range()),
            end..end
        );
        content.update(cx, |view, cx| {
            view.readonly = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let checkbox = state
            .read_with(cx, |state, _| state.range_to_bounds(&(0..5)))
            .expect("read-only task remains visible");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            "- [x] Task\n\nend"
        );
    }

    #[gpui::test]
    fn task_checkbox_is_not_painted_after_scrolling_past_it(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = format!("- [x] Task\n{}end", "line\n".repeat(400));
        let (view, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("markdown")
                    .line_number(false)
                    .folding(false)
                    .default_value(source.clone())
            });
            let content = cx.new(|_| MarkdownEditorTest {
                state,
                mode: MarkdownMode::LivePreview,
                readonly: false,
                content_padding: None,
            });
            crate::Root::new(content, window, cx)
        });
        let state = view.read_with(cx, |view, cx| {
            view.view()
                .clone()
                .downcast::<MarkdownEditorTest>()
                .unwrap()
                .read(cx)
                .state
                .clone()
        });
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(
            state
                .read_with(cx, |state, _| state.range_to_bounds(&(0..5)))
                .is_some()
        );
        let end = source.len();
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        cx.update(|window, cx| window.draw(cx).clear());
        state.read_with(cx, |state, _| {
            assert!(state.range_to_bounds(&(end..end)).is_some());
            // The task scrolled out: neither its checkbox nor any other
            // off-screen range resolves to the first visible line.
            assert_eq!(state.range_to_bounds(&(0..5)), None);
        });
    }

    #[gpui::test]
    fn range_over_a_rendered_heading_spans_its_block(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "intro\n# Heading 🌲 é\nend";
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
                content_padding: None,
            });
            crate::Root::new(content, window, cx)
        });
        let state = view.read_with(cx, |view, cx| {
            view.view()
                .clone()
                .downcast::<MarkdownEditorTest>()
                .unwrap()
                .read(cx)
                .state
                .clone()
        });
        cx.update(|window, cx| window.draw(cx).clear());
        cx.update(|window, cx| window.draw(cx).clear());
        let heading = source.find('#').unwrap()..source.find("\nend").unwrap();
        let next = source.find("end").unwrap();
        state.read_with(cx, |state, _| {
            let block = state.range_to_bounds(&heading).expect("heading laid out");
            let start = state
                .range_to_bounds(&(heading.start..heading.start))
                .unwrap();
            let next = state.range_to_bounds(&(next..next)).unwrap();
            assert_eq!(block.top(), start.top());
            // The range ends inside the block, not on the following line.
            assert!(block.bottom() <= next.top());
            assert!(block.size.height >= start.size.height);
        });
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
    fn revealed_tasks_show_checkbox_glyphs_outside_the_active_line() {
        let source = "- [x] done\n- [ ] todo\n1. [X] ordered\n> - [ ] quoted\n\nend";
        let todo = source.find("todo").unwrap();
        assert_eq!(
            projected(source, todo..todo, true),
            "\u{2003}\u{2002} done\n- [ ] todo\n1. \u{2003}\u{2002} ordered\n> \u{2003}\u{2002} quoted\n\nend"
        );
    }

    #[test]
    fn list_items_reveal_only_the_active_line() {
        let source = "- One\n- Two **bold**\n\n- [x] Open\n- [ ] Click";
        assert!(MarkdownDocument::parse(source).blocks.is_empty());
        assert_eq!(
            projected(source, 2..2, true),
            "- One\n• Two bold\n\n\u{2003}\u{2002} Open\n\u{2003}\u{2002} Click"
        );
        let click = source.find("Click").unwrap();
        assert_eq!(
            projected(source, click..click, true),
            "• One\n• Two bold\n\n\u{2003}\u{2002} Open\n- [ ] Click"
        );
    }

    #[gpui::test]
    fn clicking_a_list_item_keeps_the_other_items_in_place(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let source = "- One\n- Two\n\n- [x] Open\n- [ ] Click\n\nend";
        let (_, state, cx) = editor(cx, source);
        let click = source.find("Click").unwrap();
        let bounds = |cx: &mut VisualTestContext| {
            state.read_with(cx, |state, _| {
                state
                    .range_to_bounds(&(click..click + 1))
                    .map(|bounds| bounds.origin.y)
            })
        };
        let before = bounds(cx).unwrap();
        let one = state
            .read_with(cx, |state, _| state.range_to_bounds(&(2..3)))
            .unwrap();
        cx.simulate_click(one.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let caret = state.read_with(cx, |state, _| state.selected_range());
        assert!(
            caret.is_empty() && caret.start <= "- One".len(),
            "{caret:?}"
        );
        assert_eq!(bounds(cx).unwrap(), before);
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            source
        );
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
        assert_eq!(document.blocks.len(), 3);
        let task = document.tasks.first().unwrap();
        assert_eq!(&source[task.marker.clone()], " ");
        for block in document.blocks {
            assert!(block.range.start == 0 || source.as_bytes()[block.range.start - 1] == b'\n');
            assert!(block.range.end == source.len() || source.as_bytes()[block.range.end] == b'\n');
        }
    }
}
