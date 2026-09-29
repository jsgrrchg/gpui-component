# Display Mapping System

Layered coordinate conversion and code folding for Editor/Input.

## Architecture

```
Buffer (Rope)              Logical text
    ↓
WrapMap                    Soft-wrapping (buffer_line ↔ wrap_row)
    ↓
FoldMap                    Fold projection (wrap_row ↔ display_row)
    ↓
DisplayMap                 Public facade (BufferPos ↔ DisplayPos)
```

## Coordinate Systems

| Type | Fields | Scope | Description |
|------|--------|-------|-------------|
| `BufferPos` | `{ line, col }` | public | Logical line/column in Rope |
| `WrapPos` | `{ row, col }` | internal | Visual row after soft-wrapping |
| `DisplayPos` | `{ row, col }` | public | Final visible row after folding |

## Modules

### `DisplayMap` — Public facade

- `buffer_pos_to_display_pos()` / `display_pos_to_buffer_pos()`
- `set_fold_candidates()`, `toggle_fold()`, `is_folded_at()`, `clear_folds()`
- `on_text_changed()`, `on_layout_changed()`, `set_font()`
- `adjust_folds_for_edit()` — incremental fold/candidate line-delta adjustment
- `update_fold_candidates_for_edit()` — region-scoped candidate extraction after edits

### `WrapMap` — Soft-wrapping layer

Built on `TextWrapper`. Provides buffer ↔ wrap coordinate mapping with prefix sum cache for O(1) line lookups.

`EditorDisplayProvider` can project a line's text before wrapping. Each projected
line keeps both source-to-display and display-to-source UTF-8 byte mappings.
Text shaping, hit testing, Home/End and caret movement use those mappings;
editing, search, clipboard and undo continue to use the original rope.

Replacements are sorted and normalized once per snapshot. Each line binary-seeks
its overlapping replacements, avoiding a scan of the full replacement list.
Changing active tokens or block measurements rewraps only affected buffer rows;
the `SumTree` retains the other lines. Character references and escapes replace
only their own source spans, preserving positions in surrounding text.

Rendered blocks reserve measured visual rows on their first buffer line and
zero rows on their continuation lines. The provider reveals the original lines
when the cursor or selection touches the block. Block measurements are cached
by width and font, and visible blocks are measured again as their contents load.

### `FoldMap` — Fold projection layer

Maintains `visible_wrap_rows` and reverse mapping. When no folds are active, uses identity mapping (wrap_row == display_row) without Vec allocation.

### `FoldRange` / `folding` — Fold extraction

- `extract_fold_ranges(tree)` — full tree traversal (initial load only)
- `extract_fold_ranges_in_range(tree, byte_range)` — region-scoped traversal for edits, skips subtrees outside range
