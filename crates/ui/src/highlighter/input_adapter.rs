use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{HighlightStyle, SharedString, Task};
use gpui_base::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit as BaseInputEdit, InputHighlighter,
    InputHighlighterFactory,
};
use ropey::Rope;
use tree_sitter::{InputEdit, Point};

use super::{LanguageRegistry, SyntaxHighlighter};

pub(crate) fn input_highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        let config = LanguageRegistry::singleton().language(language)?;
        config.has_grammar().then(|| {
            Box::new(TreeSitterInputHighlighter::new(language)) as Box<dyn InputHighlighter>
        })
    })
}

struct TreeSitterInputHighlighter {
    language: SharedString,
    inner: Rc<RefCell<Option<SyntaxHighlighter>>>,
    parse_task: Option<Task<()>>,
    generation: Rc<Cell<u64>>,
    revision: u64,
}

impl TreeSitterInputHighlighter {
    fn new(language: &str) -> Self {
        Self {
            language: language.to_owned().into(),
            inner: Rc::new(RefCell::new(None)),
            parse_task: None,
            generation: Rc::new(Cell::new(0)),
            revision: LanguageRegistry::singleton().revision(),
        }
    }
}

impl Drop for TreeSitterInputHighlighter {
    fn drop(&mut self) {
        self.generation.set(self.generation.get().wrapping_add(1));
    }
}

#[cfg(test)]
impl SyntaxHighlighter {
    pub(crate) fn update_input(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        timeout: Option<std::time::Duration>,
    ) -> bool {
        self.update(edit.map(to_tree_sitter_edit), text, timeout)
    }
}

/// Publication gate shared by styles and folding. The generation prevents ABA
/// (returning to identical text, or replacing an adapter with the same language).
fn result_is_current(
    generation: u64,
    current_generation: u64,
    revision: u64,
    current_revision: u64,
    parsed_text: &Rope,
    current_text: &Rope,
) -> bool {
    generation == current_generation && revision == current_revision && parsed_text == current_text
}

impl InputHighlighter for TreeSitterInputHighlighter {
    fn language(&self) -> SharedString {
        LanguageRegistry::singleton()
            .snapshot(&self.language)
            .map(|entry| entry.config.name.clone())
            .unwrap_or_else(|| self.language.clone())
    }

    fn needs_update(&self) -> bool {
        self.revision != LanguageRegistry::singleton().revision()
    }

    fn update(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        let registry = LanguageRegistry::singleton();
        let revision = registry.revision();
        let Some(entry) = registry.snapshot(&self.language) else {
            return;
        };
        let same_configuration = self.revision == revision;
        self.revision = revision;
        let edit = edit.map(to_tree_sitter_edit);
        let (old_tree, injection_data) = {
            let mut inner = self.inner.borrow_mut();
            if !same_configuration {
                *inner = None;
            }
            if let Some(highlighter) = inner.as_mut() {
                highlighter.edit_tree(edit, text);
                // Injection trees cover disjoint source ranges. Reusing them
                // without applying this edit can silently preserve old tokens.
                (edit.and_then(|_| highlighter.tree().cloned()), None)
            } else {
                (None, None)
            }
        };
        self.generation.set(self.generation.get().wrapping_add(1));
        let generation = self.generation.get();
        let current_generation = self.generation.clone();
        let highlighter = self.inner.clone();
        let text = text.clone();
        let text_for_apply = text.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        // Drop the previous task before enqueueing work, so its parse callback
        // observes cancellation even if the replacement job starts immediately.
        self.parse_task.take();
        self.parse_task = Some(cx.spawn_in(window, async move |entity, cx| {
            struct CancelOnDrop(Arc<AtomicBool>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _cancel_guard = CancelOnDrop(cancel.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let parsed = SyntaxHighlighter::parse_background(
                        &entry,
                        &text,
                        old_tree,
                        injection_data,
                        &cancel,
                    )?;
                    let folds = if folding {
                        parsed.tree().map(extract_fold_ranges).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    Some((parsed, folds))
                })
                .await;
            if let Some((parsed, folds)) = result {
                let _ = entity.update(cx, |state, cx| {
                    // Check inside the live entity update, before publishing
                    // either styles or folds. A detached view cannot apply work.
                    if !result_is_current(
                        generation,
                        current_generation.get(),
                        revision,
                        LanguageRegistry::singleton().revision(),
                        &text_for_apply,
                        state.text(),
                    ) {
                        if LanguageRegistry::singleton().revision() != revision {
                            cx.notify();
                        }
                        return;
                    }
                    *highlighter.borrow_mut() = Some(parsed);
                    state.apply_highlighter_fold_candidates(folds, cx);
                    cx.notify();
                });
            }
        }));
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        self.inner
            .borrow()
            .as_ref()
            .map(|inner| inner.styles(range, resolver))
            .unwrap_or_else(|| vec![(range.clone(), HighlightStyle::default())])
    }
    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(|inner| inner.tree())
            .map(extract_fold_ranges)
            .unwrap_or_default()
    }

    fn fold_ranges_for_edit(&self, range: Range<usize>, _: &Rope) -> Vec<FoldRange> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(|inner| inner.tree())
            .map(|tree| extract_fold_ranges_in_range(tree, range))
            .unwrap_or_default()
    }
}

fn to_tree_sitter_edit(edit: BaseInputEdit) -> InputEdit {
    InputEdit {
        start_byte: edit.start_byte,
        old_end_byte: edit.old_end_byte,
        new_end_byte: edit.new_end_byte,
        start_position: Point::new(edit.start_position.row, edit.start_position.column),
        old_end_position: Point::new(edit.old_end_position.row, edit.old_end_position.column),
        new_end_position: Point::new(edit.new_end_position.row, edit.new_end_position.column),
    }
}

fn extract_fold_ranges(tree: &tree_sitter::Tree) -> Vec<FoldRange> {
    extract_fold_ranges_in_range(tree, 0..usize::MAX)
}

fn extract_fold_ranges_in_range(
    tree: &tree_sitter::Tree,
    byte_range: Range<usize>,
) -> Vec<FoldRange> {
    fn collect(node: tree_sitter::Node, bytes: &Range<usize>, ranges: &mut Vec<FoldRange>) {
        if node.end_byte() <= bytes.start || node.start_byte() >= bytes.end {
            return;
        }
        let start = node.start_position().row;
        let end = node.end_position().row;
        if end.saturating_sub(start) < 2 {
            return;
        }
        ranges.push(FoldRange::new(start, end));
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect(child, bytes, ranges);
        }
    }

    let root = tree.root_node();
    let mut ranges = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        collect(child, &byte_range, &mut ranges);
    }
    ranges.sort_by_key(|range| range.start_line);
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_results_cannot_publish_styles_or_folds() {
        let original = Rope::from_str("same text");
        let edited = Rope::from_str("different text");
        assert!(result_is_current(4, 4, 8, 8, &original, &original));
        assert!(!result_is_current(4, 4, 8, 8, &original, &edited));
        assert!(
            !result_is_current(4, 5, 8, 8, &original, &original),
            "ABA text/language or retired adapter"
        );
        assert!(
            !result_is_current(4, 4, 8, 9, &original, &original),
            "configuration or injection alias changed"
        );
    }

    #[test]
    fn pending_highlighter_preserves_plain_text_runs() {
        let adapter = TreeSitterInputHighlighter::new("json");
        assert_eq!(
            adapter.styles(
                &(0..12),
                super::super::HighlightTheme::default_dark().as_ref()
            ),
            vec![(0..12, HighlightStyle::default())]
        );
    }

    #[test]
    fn constructing_an_input_adapter_does_not_compile_queries() {
        let registry = LanguageRegistry::singleton();
        let config = super::super::LanguageConfig::new(
            "lazy-input-test",
            tree_sitter_json::LANGUAGE.into(),
            vec![],
            "(string) @string",
            "",
            "",
        );
        registry.register("lazy-input-test", &config);
        let entry = registry.snapshot("lazy-input-test").unwrap();
        let adapter = TreeSitterInputHighlighter::new("lazy-input-test");
        assert!(adapter.inner.borrow().is_none());
        assert!(entry.queries.get().is_none());
        let token = adapter.generation.clone();
        drop(adapter);
        assert_eq!(
            token.get(),
            1,
            "dropping the adapter invalidates queued completions"
        );
    }
}
