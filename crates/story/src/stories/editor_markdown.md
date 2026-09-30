# Markdown live preview

Write Markdown in **Source** mode or edit the formatted document in **Live preview**, just like NeverWrite. Turn on the preview pane to read both views together.

## Edit in place

Click **bold text**, *emphasis*, ~~strikethrough~~, `inline code`, or ==highlighted text== to reveal its Markdown syntax. Moving the caret away hides the markers again.

Selections reveal the source they touch. Copy, paste, search, and undo all use the original Markdown.

## Lists and tasks

- One document shared by every mode
- Unicode works too: niño, 世界, café
- Nested formatting: **_bold and italic_**

- [x] Open the Markdown editor
- [ ] Click a task checkbox
- [ ] Try switching modes after an edit

## Code

```rust
let editor = cx.new(|cx| {
    EditorState::new(window, cx)
        .language("markdown")
        .default_value("# Hello\n\n**Markdown**")
});

Editor::new(&editor).markdown_mode(MarkdownMode::LivePreview)
```

## Enlaces entre notas

Visita [[Viaje]], [[Ideas.md|ideas para el viaje]] o [[No existe|una nota pendiente]].
En Live preview, usa Ctrl+clic (Cmd+clic en macOS) para navegar; en el panel Preview basta un clic.
Las pestañas de notas permiten volver a esta demostración. Cada nota conserva sus ediciones y su historial.

![[Viaje]]

![[Vacia]]

![[No existe]]

Haz clic en el título de una tarjeta para abrir su nota, o en el contenido para editar el embed.
Los tokens escapados, como \[[Ideas]], y el código `![[Viaje]]` permanecen literales.

## Images

Images are centered and keep their proportions. Click the image to edit its source,
then move the caret outside its paragraph to show the image again.

![[/assets/paisaje-lago.png|400]]

Change `400` to `200`, or remove `|400` to use the image's natural size.
Relative paths work too: `![[assets/paisaje-lago.png|200]]`.

## Tables

| Mode | What you see | Editable |
| :--- | :--- | :---: |
| Source | Original Markdown | Yes |
| Live preview | Formatting with syntax on demand | Yes |
| Preview pane | Rendered document | Select and copy |

> Click a rendered heading, quote, code block, or table to edit its source. Move the caret outside the block to render it again.

Read more at [GPUI Component](https://longbridge.github.io/gpui-component/).

---

The Markdown source remains the saved document in every mode.
