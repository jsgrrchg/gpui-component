---
title: Editor
description: Source-code editor with syntax highlighting, gutter, folding, and decorations.
---

# Editor

`Editor` is the styled source-code control. Use [`Input`](./input.md) for
single-line values and [`Textarea`](./textarea.md) for ordinary multi-line text.

## Import

```rust
use gpui_component::input::{Editor, EditorState, TabSize};
```

## Basic usage

```rust
let editor = cx.new(|cx| {
    EditorState::new(window, cx)
        .language("rust")
        .line_number(true)
        .folding(true)
        .tab_size(TabSize {
            tab_size: 4,
            hard_tabs: false,
        })
        .default_value("fn main() {\n    println!(\"Hello\");\n}")
});

Editor::new(&editor).h(px(320.))
```

The language set via `.language()` selects syntax highlighting. Enable the
matching Cargo feature, such as `tree-sitter-rust` or `tree-sitter-markdown`;
use `tree-sitter-languages` to bundle all built-in grammars.

## Editor options

```rust
let editor = cx.new(|cx| {
    EditorState::new(window, cx)
        .language("json")
        .line_number(true)
        .folding(true)
        .show_whitespaces(true)
        .default_value(source)
});
```

## Markdown modes

Use the same `EditorState` to switch between Markdown source and an editable
live preview. A live preview hides syntax outside the active token, renders
headings, code blocks, tables, images, quotes and task lists, and reveals a
block's source when clicked or selected.

```rust
use gpui_component::input::{Editor, EditorState, MarkdownMode};

let editor = cx.new(|cx| {
    EditorState::new(window, cx)
        .language("markdown")
        .line_number(false)
        .folding(false)
        .default_value("# Notes\n\n**Hello**\n\n- [ ] Write a note")
});

Editor::new(&editor)
    .markdown_mode(MarkdownMode::LivePreview)
    .h(px(480.))
```

`MarkdownMode::Source` shows the editable source; `MarkdownMode::Preview`
provides a selectable reading view. Put a second editor with `Preview` next
to the editable one to make a live preview pane. Both read the same state.
The pane refreshes automatically when the document changes.

Formatting supports bold, italic, strikethrough, inline code, links and
`==highlight==`. Ctrl-click (Cmd-click on macOS) opens inline links. Task
checkboxes, including nested tasks and tasks inside quotes, update the original
`[ ]` / `[x]` marker and participate in undo.
Read-only mode disables task changes while keeping source selection available.

Clicking the upper half of a rendered block reveals its source at the start;
clicking the lower half places the cursor at the end.

In both editable modes, Enter continues bullets, ordered lists, task lists and
quote prefixes. New tasks start unchecked, and Enter on an empty item removes
its marker. Enter on an empty quote removes one quote level. Shift+Enter inserts
a newline without continuing a marker. Fenced code keeps normal editor behavior.

Switching modes does not replace the source, cursor, selection or undo history.
Parsing and preview rendering work without tree-sitter, including on WASM;
the Markdown grammar feature only adds source syntax highlighting.

## Decorations

```rust
let decorations = editor.update(cx, |state, cx| {
    state.create_decorations_collection(initial_decorations, cx)
});
```

Keep the returned `TextDecorationCollection` alive while the decorations are
needed. Its ranges follow subsequent text edits.

## Value and events

```rust
let source = editor.read(cx).value();

editor.update(cx, |state, cx| {
    state.set_value(new_source, window, cx);
});

cx.subscribe(&editor, |this, state, event: &InputEvent, cx| {
    if matches!(event, InputEvent::Change) {
        this.source = state.read(cx).value();
        cx.notify();
    }
});
```

## Appearance

```rust
Editor::new(&editor)
    .h(px(480.))
    .bordered(true)
    .disabled(false)
    .readonly(false)
    .aria_label("Rust source")
```

Use `readonly` to preview a file without allowing changes. Unlike `disabled`,
a read-only editor keeps the normal appearance and still can be focused,
selected, copied and searched, it only rejects the changes made by the user.
The programmatic APIs such as `set_value` keep working.

```rust
Editor::new(&editor).readonly(true)
```

Editor focus does not add the single-line Input focus-border treatment. The
gutter, current-line background, and scrollbars are painted as one aligned
editor surface.

Input-only adornments such as `prefix`, `suffix`, mask toggle, and clear button
are intentionally absent. Compose toolbars and actions around `Editor`.
