use std::sync::Arc;

use gpui::{
    App, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::{
    TextView, TextViewStyle,
    node::{ImageNode, NodeContext},
    text_view::handle_link_click,
    wiki::note_target,
};
use crate::ActiveTheme as _;

/// A resolved note. `id` is its canonical application-owned identity; aliases
/// and relative paths resolving to the same note must return the same id.
#[derive(Clone, Debug)]
pub struct MarkdownNote {
    pub id: SharedString,
    pub title: SharedString,
    pub markdown: SharedString,
}

type ResolveNote = dyn Fn(&str) -> Option<MarkdownNote> + Send + Sync;
type NavigateNote = dyn Fn(&MarkdownNote, &mut Window, &mut App) + Send + Sync;

struct NoteCallbacks {
    resolve: Box<ResolveNote>,
    navigate: Box<NavigateNote>,
}

/// Application-owned note resolution and navigation, shared by Editor and TextView.
/// Resolution runs during layout: use an in-memory index/cache, and call
/// [`Self::refreshed`] when the index or a note's Markdown changes.
#[derive(Clone)]
pub struct MarkdownNotes {
    callbacks: Arc<NoteCallbacks>,
    revision: Arc<()>,
    ancestors: Vec<SharedString>,
}

impl PartialEq for MarkdownNotes {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.revision, &other.revision) && self.ancestors == other.ancestors
    }
}

impl MarkdownNotes {
    pub fn new(
        resolve: impl Fn(&str) -> Option<MarkdownNote> + Send + Sync + 'static,
        navigate: impl Fn(&MarkdownNote, &mut Window, &mut App) + Send + Sync + 'static,
    ) -> Self {
        Self {
            callbacks: Arc::new(NoteCallbacks {
                resolve: Box::new(resolve),
                navigate: Box::new(navigate),
            }),
            revision: Arc::new(()),
            ancestors: Vec::new(),
        }
    }

    pub fn resolve(&self, target: &str) -> Option<MarkdownNote> {
        (self.callbacks.resolve)(target)
    }

    /// Return a new revision to invalidate rendered embeds after external updates.
    pub fn refreshed(&self) -> Self {
        Self {
            revision: Arc::new(()),
            ..self.clone()
        }
    }

    /// Navigation is deferred until the current input event has released its
    /// editor borrow, so the callback can safely replace that editor's document.
    pub fn navigate(&self, target: &str, window: &mut Window, cx: &mut App) {
        if let Some(note) = self.resolve(target) {
            let callbacks = self.callbacks.clone();
            window.defer(cx, move |window, cx| {
                (callbacks.navigate)(&note, window, cx)
            });
        }
    }

    fn for_embed(&self, id: &SharedString) -> Option<Self> {
        if self.ancestors.len() >= 4 || self.ancestors.contains(id) {
            return None;
        }
        let mut context = self.clone();
        context.ancestors.push(id.clone());
        Some(context)
    }
}

pub(crate) fn link_color(url: &str, style: &TextViewStyle, cx: &App) -> gpui::Hsla {
    if let Some(target) = note_target(url)
        && !style
            .notes
            .as_ref()
            .is_some_and(|notes| notes.resolve(target).is_some())
    {
        cx.theme().danger
    } else {
        cx.theme().link
    }
}

pub(crate) fn render_embed(
    id: usize,
    image: &ImageNode,
    node_cx: &NodeContext,
    cx: &mut App,
) -> gpui::AnyElement {
    let style = &node_cx.style;
    let target = note_target(&image.url).unwrap_or_default();
    let notes = style.notes.clone();
    let note = notes.as_ref().and_then(|notes| notes.resolve(target));
    let title = image
        .alt
        .clone()
        .filter(|title| title.as_ref() != target)
        .or_else(|| note.as_ref().map(|note| note.title.clone()))
        .unwrap_or_else(|| target.to_string().into());
    let nested_notes = note
        .as_ref()
        .and_then(|note| notes.as_ref()?.for_embed(&note.id));
    let status = if note.is_none() {
        Some("Note not found")
    } else if nested_notes.is_none() {
        Some("Recursive embed or nesting limit reached")
    } else if note
        .as_ref()
        .is_some_and(|note| note.markdown.trim().is_empty())
    {
        Some("Empty note")
    } else {
        None
    };
    let target = target.to_string();
    let resolved = note.is_some();
    let mut card = div()
        .id(("note-embed", id))
        .w_full()
        .min_w_0()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(px(6.))
        .text_size(px(14.))
        .font_weight(FontWeight::NORMAL)
        .line_height(px(22.))
        .text_color(cx.theme().foreground)
        .child(
            div()
                .id("note-embed-title")
                .px_3()
                .py_2()
                .bg(cx.theme().muted.opacity(0.4))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if resolved {
                    cx.theme().link
                } else {
                    cx.theme().danger
                })
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    if let Some(notes) = &notes {
                        notes.navigate(&target, window, cx);
                    }
                })
                .child(title),
        );
    if let Some(status) = status {
        card = card.child(
            div()
                .p_3()
                .text_color(cx.theme().muted_foreground)
                .child(status),
        );
    } else if let Some(note) = note {
        let nested_style = TextViewStyle {
            notes: nested_notes,
            ..style.clone()
        };
        let link_handler = node_cx.link_click_handler.clone();
        card = card.child(
            div()
                .id("note-embed-content")
                .p_3()
                .max_h(px(320.))
                .overflow_y_scroll()
                .child(
                    TextView::markdown("note-embed-preview", note.markdown)
                        .style(nested_style)
                        .markdown_extensions((*node_cx.markdown_extensions).clone())
                        .on_link_click(move |url, event, window, cx| {
                            handle_link_click(
                                &link_handler,
                                url.clone(),
                                event.clone(),
                                window,
                                cx,
                            );
                        })
                        .task_list_readonly(true)
                        .w_full(),
                ),
        );
    }
    // Padding is included in live-preview block measurements; external margins
    // would leave the reserved editor rows shorter than the visible card.
    div()
        .id(("note-embed-frame", id))
        .w_full()
        .min_w_0()
        .py(px(8.))
        .child(card)
        .into_any_element()
}
