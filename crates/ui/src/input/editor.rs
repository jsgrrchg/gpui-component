use std::{path::PathBuf, rc::Rc};

use gpui::{
    App, DefiniteLength, Entity, InteractiveElement as _, IntoElement, ParentElement, RenderOnce,
    SharedString, StyleRefinement, Styled, Window, prelude::FluentBuilder as _,
};

use super::{EditorState, Input, MarkdownMode};
use crate::native_menu::NativeMenu;
use crate::{RoleOverride, StyledExt as _};

/// A styled source-code editor.
#[derive(IntoElement)]
pub struct Editor {
    state: Entity<EditorState>,
    style: StyleRefinement,
    height: Option<DefiniteLength>,
    appearance: bool,
    bordered: bool,
    disabled: bool,
    readonly: bool,
    markdown_mode: Option<MarkdownMode>,
    markdown_image_root: Option<PathBuf>,
    markdown_notes: Option<crate::text::MarkdownNotes>,
    tab_index: isize,
    role: RoleOverride,
    aria_label: Option<SharedString>,

    /// An optional context menu builder to allow a custom context menu.
    ///
    /// If set, this overrides the built-in context menu.
    context_menu_builder: Option<Rc<dyn Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu>>,
}

impl Editor {
    pub fn new(state: &Entity<EditorState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
            height: None,
            appearance: true,
            bordered: true,
            disabled: false,
            readonly: false,
            markdown_mode: None,
            markdown_image_root: None,
            markdown_notes: None,
            tab_index: 0,
            role: RoleOverride::default(),
            aria_label: None,
            context_menu_builder: None,
        }
    }

    pub fn h(mut self, height: impl Into<DefiniteLength>) -> Self {
        self.height = Some(height.into());
        self
    }

    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }

    pub fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set the editor to read-only, default is `false`.
    ///
    /// Unlike [`Self::disabled`], a read-only editor keeps the normal appearance
    /// and still can be focused, selected and copied, it only rejects the changes
    /// made by the user.
    pub fn readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
    }

    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = index;
        self
    }

    /// Show Markdown source, a reading preview, or a NeverWrite-style editable preview.
    /// Set the state's language to `markdown` for source highlighting.
    pub fn markdown_mode(mut self, mode: MarkdownMode) -> Self {
        self.markdown_mode = Some(mode);
        self
    }

    /// Root for local Markdown images, including `![[/assets/photo.png|400]]`.
    /// Both `/assets/photo.png` and `assets/photo.png` resolve inside this root.
    /// Other Markdown views of the same state inherit this root.
    pub fn markdown_image_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.markdown_image_root = Some(root.into());
        self
    }

    /// Resolve `[[note]]` links and `![[note]]` embeds through the application's
    /// note index. Other Markdown views of this state inherit these callbacks.
    pub fn markdown_notes(mut self, notes: crate::text::MarkdownNotes) -> Self {
        self.markdown_notes = Some(notes);
        self
    }

    pub fn role(mut self, role: impl Into<RoleOverride>) -> Self {
        self.role = role.into();
        self
    }

    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    /// Replace the built-in context menu shown on right-click.
    ///
    /// The closure receives an empty menu and returns the one to show, so it
    /// decides entirely what appears — the default items are not added.
    pub fn context_menu(
        mut self,
        f: impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static,
    ) -> Self {
        self.context_menu_builder = Some(Rc::new(f));
        self
    }
}

impl Styled for Editor {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Editor {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if let Some(mode) = self.markdown_mode {
            // Keep the document's presentation session alive in every mode,
            // including Source where no display provider is attached.
            super::markdown::retain_session(&self.state, window, cx);
            if let Some(notes) = self.markdown_notes {
                super::markdown::set_notes(&self.state, notes, window, cx);
            }
            if let Some(root) = self.markdown_image_root {
                super::markdown::set_image_root(&self.state, Some(root), window, cx);
            }
            if mode == MarkdownMode::Preview {
                return gpui::div()
                    .size_full()
                    .child(super::markdown::reading_preview(&self.state, window, cx))
                    .when_some(self.height, |this, height| this.h(height))
                    .refine_style(&self.style)
                    .into_any_element();
            }
            let provider = (mode == MarkdownMode::LivePreview)
                .then(|| super::markdown::provider(&self.state, window, cx));
            self.state.update(cx, |state, cx| {
                state.set_markdown_editing(true, cx);
                state.set_display_provider(provider, cx);
            });
        }
        let task_mouse_down = (self.markdown_mode == Some(MarkdownMode::LivePreview))
            .then(|| super::markdown::task_mouse_down(&self.state, window, cx));
        let input = Input::from_state(self.state.clone())
            .appearance(self.appearance)
            .bordered(self.bordered)
            .focus_bordered(false)
            .disabled(self.disabled)
            .readonly(self.readonly)
            .tab_index(self.tab_index)
            .role(self.role)
            .when_some(self.aria_label, |this, label| this.aria_label(label))
            .when_some(self.context_menu_builder, |this, build| {
                this.context_menu(move |menu, window, cx| build(menu, window, cx))
            });
        let Some(task_mouse_down) = task_mouse_down else {
            return input
                .when_some(self.height, |this, height| this.h(height))
                .refine_style(&self.style)
                .into_any_element();
        };
        // Task checkboxes are glyphs in the text, so their clicks are caught
        // around the input before it moves the caret. The frame takes the
        // layout style; the input keeps the text style it would otherwise
        // override with its own size.
        let text_style = StyleRefinement {
            text: self.style.text.clone(),
            ..Default::default()
        };
        gpui::div()
            .size_full()
            .when_some(self.height, |this, height| this.h(height))
            .refine_style(&self.style)
            .relative()
            .capture_any_mouse_down(task_mouse_down)
            .child(input.size_full().refine_style(&text_style))
            .child(super::markdown::task_checkboxes(&self.state, window, cx))
            .into_any_element()
    }
}
