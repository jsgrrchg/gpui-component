//! Editable fenced code blocks for the Markdown live preview.
//!
//! A fenced block keeps its rendered frame while its code is edited in a
//! nested code editor. The nested editor mirrors the code lines between the
//! fences: its edits replace those lines in the document, and document
//! changes (undo, external edits) replace its text. The fences themselves are
//! revealed as source with Escape, a click on the header, or by leaving the
//! code past the start or end of the document.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, EntityId, EntityInputHandler as _,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels,
    ScrollWheelEvent, SharedString, Styled as _, Subscription, WeakEntity, Window, canvas, div,
    point, px,
};

use super::code_block;
use crate::{
    ActiveTheme as _,
    input::{Editor, EditorState, Escape, InputEvent, MoveDown, MoveLeft, MoveRight, MoveUp},
};

/// Vertical editor padding of a medium `Input`, which hosts the nested editor.
const EDITOR_PADDING_Y: f32 = 8.;

/// A fenced code block whose code lines can be edited in place.
#[derive(Clone)]
pub(super) struct FencedCode {
    /// Position among the document's fenced code blocks; keys the nested editor.
    pub(super) ordinal: usize,
    pub(super) lang: Option<SharedString>,
    pub(super) code: SharedString,
    /// The code lines between the fences, including the final newline.
    pub(super) content: Range<usize>,
}

/// Parse the code lines of a closed fence whose source matches `value`.
///
/// Fences inside containers or with normalized content are left to source
/// editing, because their lines do not map one-to-one onto the code.
pub(super) fn fenced_code(
    source: &str,
    range: &Range<usize>,
    lang: Option<&str>,
    value: &str,
    ordinal: usize,
) -> Option<FencedCode> {
    let raw = &source[range.clone()];
    let fence = raw.trim_start_matches(' ');
    let marker = *fence.as_bytes().first()?;
    let count = fence.bytes().take_while(|byte| *byte == marker).count();
    if raw.len() - fence.len() > 3 || !matches!(marker, b'`' | b'~') || count < 3 {
        return None;
    }
    let open_end = raw.find('\n')?;
    let close_start = raw.rfind('\n')? + 1;
    let closing = raw[close_start..].trim_matches(' ');
    if closing.len() < count || !closing.bytes().all(|byte| byte == marker) {
        return None;
    }
    let content = range.start + open_end + 1..range.start + close_start;
    let lines = &source[content.clone()];
    (lines.strip_suffix('\n').unwrap_or(lines) == value).then(|| FencedCode {
        ordinal,
        lang: lang.filter(|lang| !lang.is_empty()).map(SharedString::from),
        code: value.to_string().into(),
        content,
    })
}

/// The document lines holding `code`.
fn lines(code: &str) -> String {
    if code.is_empty() {
        String::new()
    } else {
        format!("{code}\n")
    }
}

/// Which fenced block shows its source, and which should take focus.
#[derive(Default)]
pub(super) struct CodeFocus {
    revealed: Option<usize>,
    pending: Option<(usize, usize)>,
    // Retain visited editors independently of viewport element-state eviction.
    editors: HashMap<u64, Entity<CodeEditor>>,
}

impl CodeFocus {
    #[cfg(test)]
    pub(super) fn editor_ids(&self) -> Vec<(u64, EntityId)> {
        let mut ids = self
            .editors
            .iter()
            .map(|(id, editor)| (*id, editor.entity_id()))
            .collect::<Vec<_>>();
        ids.sort_by_key(|(id, _)| *id);
        ids
    }

    #[cfg(test)]
    pub(super) fn first_editor(&self, cx: &App) -> Option<Entity<EditorState>> {
        self.editors
            .iter()
            .min_by_key(|(id, _)| *id)
            .map(|(_, editor)| editor)
            .map(|editor| editor.read(cx).editor.clone())
    }

    pub(super) fn retain_editors(&mut self, ids: &HashSet<u64>) {
        self.editors.retain(|id, _| ids.contains(id));
    }

    /// Whether a fenced block shows its Markdown source for the document
    /// `selection`. A focused caret entering the block moves into its nested
    /// editor instead, unless the source was revealed explicitly.
    pub(super) fn shows_source(
        &mut self,
        code: &FencedCode,
        selection: &Range<usize>,
        inside: bool,
        focused: bool,
    ) -> bool {
        if !inside {
            if self.revealed == Some(code.ordinal) {
                self.revealed = None;
            }
            return false;
        }
        if !focused {
            return false;
        }
        if !selection.is_empty() || self.revealed == Some(code.ordinal) {
            return true;
        }
        let offset = selection
            .start
            .saturating_sub(code.content.start)
            .min(code.code.len());
        self.pending = Some((code.ordinal, offset));
        false
    }
}

struct CodeEditor {
    editor: Entity<EditorState>,
    document: WeakEntity<EditorState>,
    lang: Option<SharedString>,
    /// The document offset of the code lines and the code they held.
    synced: Option<(usize, SharedString)>,
    /// Whether long lines wrap to the block's width instead of scrolling.
    fit_content: bool,
    /// Visual rows of the wrapped code, measured after the last paint.
    fit_rows: Option<usize>,
    _subscription: Subscription,
}

impl CodeEditor {
    fn new(
        document: WeakEntity<EditorState>,
        lang: Option<SharedString>,
        code: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(lang.clone().unwrap_or_else(|| "text".into()))
                .line_number(false)
                .folding(false)
                .soft_wrap(false)
                .scroll_beyond_last_line(Some(0))
                .default_value(code)
        });
        let subscription = cx.subscribe_in(&editor, window, Self::on_editor_event);
        Self {
            editor,
            document,
            lang,
            synced: None,
            fit_content: false,
            fit_rows: None,
            _subscription: subscription,
        }
    }

    fn toggle_fit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fit_content = !self.fit_content;
        self.fit_rows = None;
        let fit_content = self.fit_content;
        self.editor.update(cx, |editor, cx| {
            editor.set_soft_wrap(fit_content, window, cx);
            editor.set_scroll_offset(point(px(0.), px(0.)), cx);
        });
    }

    /// Record the wrapped code's visual rows after the nested editor painted.
    /// Returns whether the block must be laid out again.
    fn measure_fit(&mut self, rows: usize, cx: &App) -> bool {
        if !self.fit_content {
            return false;
        }
        let editor = self.editor.read(cx);
        let end = editor.value().len();
        let measured = match (
            editor.range_to_bounds(&(0..0)),
            editor.range_to_bounds(&(end..end)),
            editor.line_height(),
        ) {
            (Some(first), Some(last), Some(line_height)) => {
                ((last.bottom() - first.top()) / line_height).round() as usize
            }
            // The last row lies below the viewport: grow until it fits.
            (Some(_), None, Some(_)) => rows * 2,
            _ => return false,
        };
        if self.fit_rows == Some(measured) {
            return false;
        }
        self.fit_rows = Some(measured);
        true
    }

    /// Mirror the document's code into the nested editor.
    fn sync(&mut self, code: &FencedCode, window: &mut Window, cx: &mut Context<Self>) {
        self.synced = Some((code.content.start, code.code.clone()));
        if self.lang != code.lang {
            self.lang = code.lang.clone();
            let lang = self.lang.clone().unwrap_or_else(|| "text".into());
            self.editor
                .update(cx, |editor, cx| editor.set_highlighter(lang, cx));
        }
        self.editor.update(cx, |editor, cx| {
            if editor.text().slice(0..editor.text().len()) == code.code.as_ref() {
                return;
            }
            let selection = editor.selected_range();
            editor.set_value(code.code.clone(), window, cx);
            let clamp = |offset: usize| {
                let mut offset = offset.min(code.code.len());
                while !code.code.is_char_boundary(offset) {
                    offset -= 1;
                }
                offset
            };
            editor.set_selected_range(clamp(selection.start)..clamp(selection.end), cx);
        });
    }

    fn on_editor_event(
        &mut self,
        editor: &Entity<EditorState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Some((start, previous)) = self.synced.clone() else {
            return;
        };
        let (code, selection) =
            editor.read_with(cx, |editor, _| (editor.value(), editor.selected_range()));
        let Some(document) = self.document.upgrade() else {
            return;
        };
        if code == previous {
            return;
        }
        let replaced = document.update(cx, |document, cx| {
            let previous = lines(&previous);
            let end = start + previous.len();
            if end > document.text().len() || document.text().slice(start..end) != previous.as_str()
            {
                // The document changed underneath; the next render resyncs.
                return false;
            }
            let range =
                document.text().byte_to_utf16_idx(start)..document.text().byte_to_utf16_idx(end);
            document.replace_text_in_range(Some(range), &lines(&code), window, cx);
            document.set_selected_range(start + selection.start..start + selection.end, cx);
            true
        });
        if replaced {
            self.synced = Some((start, code));
        }
    }
}

/// Everything a live block needs to render an editable fenced code block.
pub(super) struct EditableCode {
    pub(super) document: WeakEntity<EditorState>,
    pub(super) document_id: EntityId,
    pub(super) id: u64,
    pub(super) code: FencedCode,
    /// The block's complete source lines, fences included.
    pub(super) lines: Range<usize>,
    pub(super) focus: Rc<RefCell<CodeFocus>>,
}

impl EditableCode {
    pub(super) fn height_hint(&self, window: &Window, cx: &App) -> Pixels {
        let line_height = code_block::line_height(cx);
        let rows = self.code.code.split('\n').count();
        (line_height * rows as f32 + px(EDITOR_PADDING_Y * 2.)).ceil()
            + px(code_block::HEADER_HEIGHT + 2.)
            + window.rem_size() * 0.5
    }

    /// Leave the nested editor, placing the document caret at `offset`, or
    /// reveal the block's source at `fallback` when `offset` is unavailable.
    fn exit(&self, offset: Option<usize>, fallback: usize, window: &mut Window, cx: &mut App) {
        let Some(document) = self.document.upgrade() else {
            return;
        };
        let offset = offset.unwrap_or_else(|| {
            self.focus.borrow_mut().revealed = Some(self.code.ordinal);
            fallback
        });
        document.update(cx, |document, cx| {
            document.set_selected_range(offset..offset, cx);
            document.focus(window, cx);
        });
    }

    pub(super) fn render(self: &Rc<Self>, window: &mut Window, cx: &mut App) -> AnyElement {
        let code = &self.code;
        let editor = self
            .focus
            .borrow_mut()
            .editors
            .entry(self.id)
            .or_insert_with(|| {
                cx.new(|cx| {
                    CodeEditor::new(
                        self.document.clone(),
                        code.lang.clone(),
                        code.code.clone(),
                        window,
                        cx,
                    )
                })
            })
            .clone();
        editor.update(cx, |editor, cx| editor.sync(code, window, cx));
        let nested = editor.read(cx).editor.clone();
        #[cfg(test)]
        tests::LAST_EDITOR.with(|last| *last.borrow_mut() = Some(nested.clone()));

        let pending = self
            .focus
            .borrow_mut()
            .pending
            .take_if(|(ordinal, _)| *ordinal == code.ordinal);
        if let Some((_, offset)) = pending {
            let nested = nested.clone();
            window.defer(cx, move |window, cx| {
                nested.update(cx, |nested, cx| {
                    nested.set_selected_range(offset..offset, cx);
                    nested.focus(window, cx);
                });
            });
        }

        let editable = self
            .document
            .upgrade()
            .is_some_and(|document| document.read(cx).is_editable());
        // Size the editor from the line height it actually laid out, so its
        // viewport holds every row and nothing is left to scroll vertically.
        let line_height = nested
            .read(cx)
            .line_height()
            .unwrap_or_else(|| code_block::line_height(cx));
        let lines = code.code.split('\n').count();
        let (fit_content, rows) = editor.read_with(cx, |editor, _| {
            if editor.fit_content {
                (true, editor.fit_rows.unwrap_or(lines).max(lines))
            } else {
                (false, lines)
            }
        });
        let start = self.lines.start;
        let bounds = Rc::new(std::cell::Cell::new(Bounds::<Pixels>::default()));

        div()
            .id(SharedString::from(format!(
                "markdown-code-{:?}-{}",
                self.document_id, code.ordinal
            )))
            .debug_selector(move || format!("markdown-live-block-{start}"))
            .relative()
            .w_full()
            .py_1()
            .child(code_block::frame(
                start,
                code.lang.clone(),
                code.code.clone(),
                Some(code_block::fit_button(start, fit_content, {
                    let editor = editor.clone();
                    move |window, cx| {
                        editor.update(cx, |editor, cx| editor.toggle_fit(window, cx));
                        window.refresh();
                    }
                })),
                div()
                    .debug_selector(move || format!("markdown-code-editor-{start}"))
                    .relative()
                    .w_full()
                    // The nested editor owns clicks inside the code.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // A vertical wheel with Shift scrolls long lines, for mice
                    // without a horizontal wheel. Plain vertical wheels keep
                    // scrolling the document.
                    .on_scroll_wheel({
                        let nested = nested.clone();
                        move |event: &ScrollWheelEvent, _, cx| {
                            let delta = event.delta.pixel_delta(line_height);
                            if !event.modifiers.shift || delta.y.abs() <= delta.x.abs() {
                                return;
                            }
                            nested.update(cx, |nested, cx| {
                                let offset = nested.scroll_offset();
                                let x = offset.x + delta.y;
                                if x.min(px(0.)) != offset.x {
                                    cx.stop_propagation();
                                    nested.set_scroll_offset(point(x, offset.y), cx);
                                }
                            });
                        }
                    })
                    .capture_action({
                        let this = self.clone();
                        let nested = nested.clone();
                        move |_: &MoveUp, window, cx| {
                            let (text, caret) = caret(&nested, cx);
                            if caret.is_empty() && !text[..caret.start].contains('\n') {
                                cx.stop_propagation();
                                this.exit_before(window, cx);
                            }
                        }
                    })
                    .capture_action({
                        let this = self.clone();
                        let nested = nested.clone();
                        move |_: &MoveLeft, window, cx| {
                            if caret(&nested, cx).1 == (0..0) {
                                cx.stop_propagation();
                                this.exit_before(window, cx);
                            }
                        }
                    })
                    .capture_action({
                        let this = self.clone();
                        let nested = nested.clone();
                        move |_: &MoveDown, window, cx| {
                            let (text, caret) = caret(&nested, cx);
                            if caret.is_empty() && !text[caret.end..].contains('\n') {
                                cx.stop_propagation();
                                this.exit_after(window, cx);
                            }
                        }
                    })
                    .capture_action({
                        let this = self.clone();
                        let nested = nested.clone();
                        move |_: &MoveRight, window, cx| {
                            let (text, caret) = caret(&nested, cx);
                            if caret.is_empty() && caret.end == text.len() {
                                cx.stop_propagation();
                                this.exit_after(window, cx);
                            }
                        }
                    })
                    .capture_action({
                        let this = self.clone();
                        let nested = nested.clone();
                        move |_: &Escape, window, cx| {
                            cx.stop_propagation();
                            let offset = this.code.content.start + caret(&nested, cx).1.start;
                            this.exit(None, offset, window, cx);
                        }
                    })
                    .child(
                        Editor::new(&nested)
                            .appearance(false)
                            .bordered(false)
                            .readonly(!editable)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(cx.theme().mono_font_size)
                            .line_height(line_height)
                            .h((line_height * rows as f32 + px(EDITOR_PADDING_Y * 2.)).ceil()),
                    )
                    // Wrapped rows are known only once the nested editor has
                    // laid out at the block's width.
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |_, _, window, cx| {
                                if editor.update(cx, |editor, cx| editor.measure_fit(rows, cx)) {
                                    window.refresh();
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
                cx,
            ))
            .child(
                canvas(
                    {
                        let bounds = bounds.clone();
                        move |measured, _, _| bounds.set(measured)
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            // A click on the header reveals the fences, e.g. to change the language.
            .on_mouse_down(MouseButton::Left, {
                let this = self.clone();
                move |event, window, cx| {
                    cx.stop_propagation();
                    let anchor = if event.position.y <= bounds.get().center().y {
                        this.lines.start
                    } else {
                        this.lines.end
                    };
                    this.exit(None, anchor, window, cx);
                }
            })
            .into_any_element()
    }

    fn exit_before(&self, window: &mut Window, cx: &mut App) {
        let offset = self.lines.start.checked_sub(1);
        self.exit(offset, self.lines.start, window, cx);
    }

    fn exit_after(&self, window: &mut Window, cx: &mut App) {
        let len = self
            .document
            .upgrade()
            .map_or(0, |document| document.read(cx).text().len());
        let offset = (self.lines.end < len).then_some(self.lines.end + 1);
        self.exit(offset, self.lines.end, window, cx);
    }
}

fn caret(editor: &Entity<EditorState>, cx: &App) -> (SharedString, Range<usize>) {
    let editor = editor.read(cx);
    (editor.value(), editor.selected_range())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static LAST_EDITOR: RefCell<Option<Entity<EditorState>>> = const { RefCell::new(None) };
    }

    fn parse(source: &str) -> Option<FencedCode> {
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
        let markdown::mdast::Node::Code(code) = &root.children().unwrap()[0] else {
            panic!("expected code");
        };
        let position = code.position.as_ref().unwrap();
        fenced_code(
            source,
            &(position.start.offset..position.end.offset),
            code.lang.as_deref(),
            &code.value,
            0,
        )
    }

    #[test]
    fn fenced_code_maps_the_lines_between_fences() {
        let code = parse("```rust\nlet niño = \"世界\";\n```").unwrap();
        assert_eq!(code.lang.as_deref(), Some("rust"));
        assert_eq!(code.code.as_ref(), "let niño = \"世界\";");
        assert_eq!(code.content, 8..8 + "let niño = \"世界\";\n".len());

        let empty = parse("~~~\n~~~").unwrap();
        assert_eq!((empty.code.as_ref(), empty.content), ("", 4..4));
        assert_eq!(lines(""), "");
        assert_eq!(lines("a\nb"), "a\nb\n");
    }

    /// Scroll long code lines with a wheel, click the top-left corner of the
    /// code, and return the offset within the code where typing lands.
    fn offset_after_wheel(
        cx: &mut gpui::TestAppContext,
        delta_x: f32,
        delta_y: f32,
        shift: bool,
    ) -> usize {
        use crate::input::markdown::tests::{editor, redraw};
        use gpui::{Modifiers, ScrollDelta};

        cx.update(crate::init);
        let code = vec!["x".repeat(400); 3].join("\n");
        let source = format!("intro\n\n```\n{code}\n```\n\nend");
        let (_, state, cx) = editor(cx, &source);
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        cx.simulate_event(ScrollWheelEvent {
            position: body.origin + point(px(20.), px(20.)),
            delta: ScrollDelta::Pixels(point(px(delta_x), px(delta_y))),
            modifiers: Modifiers {
                shift,
                ..Default::default()
            },
            ..Default::default()
        });
        redraw(cx);
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        cx.simulate_click(
            point(
                body.origin.x + px(12.),
                body.origin.y + px(EDITOR_PADDING_Y + 3.),
            ),
            Modifiers::default(),
        );
        redraw(cx);
        cx.simulate_input("Z");
        redraw(cx);
        let value = state.read_with(cx, |state, _| state.value());
        value.find('Z').unwrap() - "intro\n\n```\n".len()
    }

    #[gpui::test]
    fn code_editor_viewport_fits_every_row(cx: &mut gpui::TestAppContext) {
        use crate::input::markdown::tests::{editor, redraw};
        use gpui::{Modifiers, ScrollDelta};

        cx.update(crate::init);
        let (_, _, cx) = editor(cx, "intro\n\n```rust\none\ntwo\nthree\nfour\n```\n\nend");
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        cx.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-200.))),
            modifiers: Modifiers::default(),
            ..Default::default()
        });
        redraw(cx);
        let nested = LAST_EDITOR.with(|last| last.borrow().clone()).unwrap();
        assert_eq!(
            nested.read_with(cx, |nested, _| nested.scroll_offset()),
            point(px(0.), px(0.))
        );
    }

    #[gpui::test]
    fn horizontal_wheels_scroll_long_lines(cx: &mut gpui::TestAppContext) {
        assert!((10..400).contains(&offset_after_wheel(cx, -200., 0., false)));
    }

    #[gpui::test]
    fn shift_wheels_scroll_long_lines(cx: &mut gpui::TestAppContext) {
        assert!((10..400).contains(&offset_after_wheel(cx, 0., -200., true)));
    }

    #[gpui::test]
    fn fit_wraps_long_lines_to_the_block_and_back(cx: &mut gpui::TestAppContext) {
        use crate::input::markdown::tests::{editor, redraw};
        use gpui::{Modifiers, ScrollDelta};

        cx.update(crate::init);
        let long = "word ".repeat(80);
        let source = format!("intro\n\n```\n{long}\n```\n\nend");
        let (_, state, cx) = editor(cx, &source);
        let single = cx
            .debug_bounds("markdown-code-editor-7")
            .unwrap()
            .size
            .height;
        let fit = cx
            .debug_bounds("markdown-editor-code-block-7-fit")
            .expect("the header must offer fit content");
        cx.simulate_click(fit.center(), Modifiers::default());
        for _ in 0..4 {
            redraw(cx);
        }
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        assert!(body.size.height > single * 3., "{single:?} -> {body:?}");
        let nested = LAST_EDITOR.with(|last| last.borrow().clone()).unwrap();
        let line_height = nested.read_with(cx, |nested, _| nested.line_height().unwrap());
        let rows = ((body.size.height - px(EDITOR_PADDING_Y * 2.)) / line_height).round();
        let last = nested
            .read_with(cx, |nested, _| {
                nested.range_to_bounds(&(long.len()..long.len()))
            })
            .expect("the last wrapped row must be visible");
        assert!(
            last.bottom() <= body.bottom(),
            "{rows} rows, last {last:?}, body {body:?}"
        );

        // Wrapped code no longer scrolls sideways, and the source is unchanged.
        cx.simulate_event(ScrollWheelEvent {
            position: body.origin + point(px(20.), px(20.)),
            delta: ScrollDelta::Pixels(point(px(-200.), px(0.))),
            ..Default::default()
        });
        redraw(cx);
        assert_eq!(
            nested.read_with(cx, |nested, _| nested.scroll_offset()),
            point(px(0.), px(0.))
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.value()).as_ref(),
            source
        );

        let fit = cx.debug_bounds("markdown-editor-code-block-7-fit").unwrap();
        cx.simulate_click(fit.center(), Modifiers::default());
        for _ in 0..4 {
            redraw(cx);
        }
        let body = cx.debug_bounds("markdown-code-editor-7").unwrap();
        assert_eq!(body.size.height, single);
    }

    #[test]
    fn unclosed_or_indented_fences_stay_source_edited() {
        assert!(parse("```\nopen").is_none());
        assert!(parse("  ```\n  code\n  ```").is_none());
        assert!(parse("````\ncode\n```").is_none());
    }
}
