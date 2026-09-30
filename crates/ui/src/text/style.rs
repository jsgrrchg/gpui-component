use std::{path::PathBuf, sync::Arc};

use gpui::{App, HighlightStyle, Pixels, Rems, StyleRefinement, px, rems};

use crate::{ActiveTheme as _, highlighter::HighlightTheme};

/// Appearance and column layout of Markdown tables.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAppearance {
    /// Bordered cells inside a rounded frame.
    #[default]
    Bordered,
    /// Frameless rows in bundled Geist with a bold header, horizontal rules,
    /// and content-proportional columns.
    /// Columns wrap down to their minimum widths, then scroll horizontally.
    Plain,
}

/// TextViewStyle used to customize the style for [`TextView`].
#[derive(Clone)]
pub struct TextViewStyle {
    /// Gap of each paragraphs, default is 1 rem.
    pub paragraph_gap: Rems,
    /// Base font size for headings, default is 14px.
    pub heading_base_font_size: Pixels,
    /// Function to calculate heading font size based on heading level (1-6).
    ///
    /// The first parameter is the heading level (1-6), the second parameter is the base font size.
    /// The second parameter is the base font size.
    pub heading_font_size: Option<Arc<dyn Fn(u8, Pixels) -> Pixels + Send + Sync + 'static>>,
    /// Highlight theme for code blocks. Default: [`HighlightTheme::default_light()`]
    pub highlight_theme: Arc<HighlightTheme>,
    /// The style refinement for code blocks.
    pub code_block: StyleRefinement,
    /// Style refinement applied to the table container (the bordered wrapper
    /// in wrap mode, the scroll viewport in horizontal-scroll mode).
    ///
    /// Set `overflow_x: scroll` here for adaptive table layout: columns fit
    /// their content when space allows, shrink (wrapping cell text) down to a
    /// per-column floor when the frame is narrower, and below that the table
    /// scrolls horizontally instead of squeezing further, e.g.
    /// `TextViewStyle::default().table({ let mut s = StyleRefinement::default(); s.overflow.x = Some(Overflow::Scroll); s })`.
    pub table: StyleRefinement,
    /// Style refinement applied to each table cell.
    ///
    /// With the scroll layout, set `white_space: nowrap` here to keep cells
    /// on a single line — columns then never shrink and the table scrolls as
    /// soon as the content is wider than the frame.
    pub table_cell: StyleRefinement,
    /// Table appearance, default is [`TableAppearance::Bordered`].
    pub table_appearance: TableAppearance,
    /// Explicit root for local Markdown images. `/assets/a.png` is relative
    /// to this root; relative paths also start here. Default: URI images only.
    pub image_root: Option<PathBuf>,
    /// Resolve and navigate wiki note links and render note embeds.
    pub notes: Option<super::MarkdownNotes>,
    /// The highlight style for inline code.
    ///
    /// Default is [`HighlightStyle::default()`], the `background_color` will
    /// fallback to `cx.theme().accent`, if it is `None`.
    pub inline_code: HighlightStyle,
    pub is_dark: bool,
}

impl PartialEq for TextViewStyle {
    fn eq(&self, other: &Self) -> bool {
        self.paragraph_gap == other.paragraph_gap
            && self.heading_base_font_size == other.heading_base_font_size
            && match (&self.heading_font_size, &other.heading_font_size) {
                (Some(left), Some(right)) => (1..=6).all(|level| {
                    left(level, self.heading_base_font_size)
                        == right(level, other.heading_base_font_size)
                }),
                (None, None) => true,
                _ => false,
            }
            && self.highlight_theme == other.highlight_theme
            && self.code_block == other.code_block
            && self.table == other.table
            && self.table_cell == other.table_cell
            && self.table_appearance == other.table_appearance
            && self.image_root == other.image_root
            && self.notes == other.notes
            && self.inline_code == other.inline_code
            && self.is_dark == other.is_dark
    }
}

impl Default for TextViewStyle {
    fn default() -> Self {
        Self {
            paragraph_gap: rems(1.),
            heading_base_font_size: px(14.),
            heading_font_size: None,
            highlight_theme: HighlightTheme::default_light().clone(),
            code_block: StyleRefinement::default(),
            table: StyleRefinement::default(),
            table_cell: StyleRefinement::default(),
            table_appearance: TableAppearance::default(),
            image_root: None,
            notes: None,
            inline_code: HighlightStyle::default(),
            is_dark: false,
        }
    }
}

impl TextViewStyle {
    pub fn notes(mut self, notes: super::MarkdownNotes) -> Self {
        self.notes = Some(notes);
        self
    }
    /// Resolve local Markdown image paths inside an application-owned root.
    pub fn image_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.image_root = Some(root.into());
        self
    }

    /// Set paragraph gap, default is 1 rem.
    pub fn paragraph_gap(mut self, gap: Rems) -> Self {
        self.paragraph_gap = gap;
        self
    }

    pub fn heading_font_size<F>(mut self, f: F) -> Self
    where
        F: Fn(u8, Pixels) -> Pixels + Send + Sync + 'static,
    {
        self.heading_font_size = Some(Arc::new(f));
        self
    }

    /// Set style for code blocks.
    pub fn code_block(mut self, style: StyleRefinement) -> Self {
        self.code_block = style;
        self
    }

    /// Set style for inline code spans.
    pub fn inline_code(mut self, style: HighlightStyle) -> Self {
        self.inline_code = style;
        self
    }

    /// Set extra style for the table container.
    ///
    /// Set `overflow_x: scroll` on the refinement for adaptive layout: cells
    /// wrap as the frame narrows, and once columns reach their minimum width
    /// the table scrolls horizontally instead of shrinking further.
    pub fn table(mut self, style: StyleRefinement) -> Self {
        self.table = style;
        self
    }

    /// Set extra style for each table cell.
    ///
    /// With the scroll table layout, `white_space: nowrap` here keeps cells
    /// on a single line and the table scrolls whenever the content is wider
    /// than the frame.
    pub fn table_cell(mut self, style: StyleRefinement) -> Self {
        self.table_cell = style;
        self
    }

    /// Set the table appearance and column layout.
    pub fn table_appearance(mut self, appearance: TableAppearance) -> Self {
        self.table_appearance = appearance;
        self
    }

    /// Returns the [`HighlightStyle`] to use for inline code,
    /// fallback `background_color` to `cx.theme().accent`, if it is `None`.
    pub(crate) fn inline_code_highlight(&self, cx: &App) -> HighlightStyle {
        let mut style = self.inline_code;
        if style.background_color.is_none() {
            style.background_color = Some(cx.theme().accent);
        }
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_layout_fingerprint_covers_callback_table_and_theme_fields() {
        let base = TextViewStyle::default();
        let heading = base.clone().heading_font_size(|_, size| size);
        assert!(heading == base.clone().heading_font_size(|_, size| size));
        assert!(heading != base.clone().heading_font_size(|_, size| size * 2.));

        let mut table = StyleRefinement::default();
        table.text.white_space = Some(gpui::WhiteSpace::Nowrap);
        assert!(base != base.clone().table_cell(table));

        let mut dark = base.clone();
        dark.is_dark = true;
        assert!(base != dark);
    }

    #[test]
    fn cloning_preserves_the_same_heading_callback_fingerprint() {
        let style = TextViewStyle::default().heading_font_size(|_, size| size);
        assert!(style == style.clone());
    }
}
