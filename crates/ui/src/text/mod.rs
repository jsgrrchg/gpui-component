pub(crate) mod advanced;
mod document;
mod format;
mod inline;
mod inline_flow;
mod markdown_ext;
mod node;
mod notes;
pub(crate) mod selection;
mod selection_adapter;
mod state;
mod style;
mod text_view;
mod utils;
pub(crate) mod wiki;
#[cfg(test)]
mod window_selection;

use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString, Window};
pub use markdown_ext::*;
pub(crate) use node::{CodeBlock, Span};
pub use notes::{MarkdownNote, MarkdownNotes};
pub use state::*;
pub use style::*;
pub use text_view::*;

pub(crate) fn init(cx: &mut App) {
    // Use actual faces, as Comet does: GPUI's platform font fallback can
    // discard a requested weight when the inherited family is unavailable.
    let fonts: Vec<std::borrow::Cow<'static, [u8]>> = [
        include_bytes!("../../assets/fonts/Geist.ttf").as_slice(),
        include_bytes!("../../assets/fonts/Geist-Bold.ttf").as_slice(),
        include_bytes!("../../assets/fonts/Geist-Italic.ttf").as_slice(),
        include_bytes!("../../assets/fonts/Geist-BoldItalic.ttf").as_slice(),
    ]
    .into_iter()
    .map(std::borrow::Cow::Borrowed)
    .collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "failed to register Markdown table fonts");
    }
    state::init(cx);
}

/// Create a new markdown text view with code location as id.
#[track_caller]
pub fn markdown(source: impl Into<SharedString>) -> TextView {
    let id: ElementId = ElementId::CodeLocation(*std::panic::Location::caller());
    TextView::markdown(id, source)
}

/// Create a new html text view with code location as id.
#[track_caller]
pub fn html(source: impl Into<SharedString>) -> TextView {
    let id: ElementId = ElementId::CodeLocation(*std::panic::Location::caller());
    TextView::html(id, source)
}

#[derive(IntoElement, Clone)]
pub enum Text {
    String(SharedString),
    TextView(Box<TextView>),
}

impl From<SharedString> for Text {
    fn from(s: SharedString) -> Self {
        Self::String(s)
    }
}

impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Self::String(SharedString::from(s.to_string()))
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Self::String(s.into())
    }
}

impl From<TextView> for Text {
    fn from(e: TextView) -> Self {
        Self::TextView(Box::new(e))
    }
}

impl Text {
    /// Set the style for [`TextView`].
    ///
    /// Do nothing if this is `String`.
    pub fn style(self, style: TextViewStyle) -> Self {
        match self {
            Self::String(s) => Self::String(s),
            Self::TextView(e) => Self::TextView(Box::new(e.style(style))),
        }
    }

    /// Get the text content.
    pub(crate) fn get_text(&self, cx: &App) -> SharedString {
        match self {
            Self::String(s) => s.clone(),
            Self::TextView(view) => {
                if let Some(state) = &view.state {
                    state.read(cx).source()
                } else {
                    SharedString::default()
                }
            }
        }
    }
}

impl RenderOnce for Text {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        match self {
            Self::String(s) => s.into_any_element(),
            Self::TextView(e) => e.into_any_element(),
        }
    }
}
