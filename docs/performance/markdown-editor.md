# Markdown editor initialization and frame work

## Configuration and query lifetime

`LanguageRegistry` stores immutable configuration snapshots. Identical complete
configurations share compiled host queries, injection queries and capture
metadata through `OnceLock`/`Arc`; compilation never holds the registry mutex.
Parser instances and trees remain independent for each editor.

A real registration or replacement advances the registry revision. Re-registering
an identical mapping does not. Built-in aliases resolve the current canonical
mapping; explicitly registered aliases can override it independently. Grammar,
highlights, locals, injections and injection-language metadata all participate in
configuration equality. Failed compilations are cached for that snapshot, and a
replacement gets a new snapshot. Injected code uses the grammar and query from
its resolved snapshot, including custom aliases whose display name matches an
existing language.

## UI scheduling and publication

The input adapter starts without compiling queries or initializing a language
parser. Compilation, host parsing, injection parsing and fold collection run on
the background executor. Markdown reading views also schedule code highlighting
on workers. Plain text remains visible while syntax styles are pending.

Input completions publish styles and folds together, only when the adapter
iteration, registry revision and source text still match. Dropping/replacing the
adapter invalidates its iteration. Entity updates prevent a retired editor from
publishing work. Reading-view jobs additionally check their theme and retain only
a weak reference to the code style cache. Obsolete configuration results request
a fresh frame so the current configuration can be prepared.

Foreground edits adjust the host tree without parsing. Unedited injection trees
are discarded before querying new source offsets. Parsing checks cancellation,
including between injection matches and during injection parsing. Query
compilation itself cannot be interrupted; it is shared and runs outside the UI.

The public `SyntaxHighlighter::new`/`update` APIs remain synchronous for direct
callers. Framework rendering uses the asynchronous adapters. Existing injection
budgets and folding/highlight functionality are retained.

## Geometry and nested editors

Projection reuses valid measured sizes even for visible blocks. Initial editable
code-block heights come from their known row count, code line height, header,
borders and padding, without creating editors outside the viewport. Visible
layout replaces the hint with its actual measurement; the existing projection
and scroll anchoring handle the resulting row count.

Visited code editors are retained by stable Markdown block identity through
scrolling and Source/Live Preview/Preview transitions. Editing a fence's code
preserves that identity. Deleted blocks release their editors. Code synchronization
compares Rope slices rather than copying the entire parent document, and keeps
both selection endpoints when refreshing the nested buffer.

API additions are optional: `InputHighlighter::needs_update` defaults to `false`,
and `EditorDisplayBlockCache::set_height_hint` supplies initial geometry. Existing
factory/provider implementations need no changes. Highlighting now arrives
asynchronously, so callers inspecting rendered style caches should wait for
background work before asserting final colors.

## Verification and measurement scope

```sh
cargo test -p gpui-base -p gpui-component \
  --features gpui-component/tree-sitter-languages --lib
cargo check -p gpui-component --no-default-features
```

Coverage includes shared queries across concurrent instances, configuration
replacement, built-in and explicit aliases, injection query replacement,
background cancellation, stale-result publication, plain text before highlighting,
64 fences with only visible editors constructed, session/editor retention across
modes and edits, Undo/Redo, task focus, Unicode source ranges, wrapping, scrolling,
selection and composition regressions.

The reported application baseline is a 219,682-byte UTF-8 note with 64 TypeScript
fences and SHA256
`9109397318d882b7fc4a3e4214a64b0ba52666fc4dfff8f15e843ddb5cc60f89`.
NeverWrite's isolated X11 baseline took 18.48 seconds to open it. Application
integration will measure the published commit using that application's original
dev profile, including autosave and restart. This repository's optimized
Tree-sitter dev package settings are not equivalent to that baseline.

An auxiliary X11/Xvfb component harness uses a verified copy of the note, isolated
outputs and mouse controls after synthetic function keys proved unreliable. It
measures action-to-paint time and verifies source snapshots. It excludes autosave,
application startup/restoration, and time to complete all background highlighting;
its append operation is a single 24-character edit rather than 24 keyboard events.
The harness and raw artifacts are local under `target/markdown-performance` and
are outside this change. They must not be presented as NeverWrite integration
results. The original note, vault and NeverWrite repository are not modified.

### Exploratory component comparison

One fresh-process run per revision, `85ed12f1` versus these changes, on the same
X11/Xvfb setup and repository dev profile (Tree-sitter `opt-level = 3`, version
0.26.8; ZUI `3151ad1`). Mesa reported DRI3 unavailable. Times below are measured
inside the component from action handling to its paint callback; mouse/snapshot
polling delays are excluded. These are observations, not statistical estimates
or measurements of NeverWrite autosave.

| Action | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| First Live Preview open | 5247.0 ms | 795.6 ms | 84.8% |
| Append 24 characters as one edit | 104.3 ms | 88.3 ms | 15.3% |
| Undo | 93.9 ms | 89.5 ms | 4.7% |
| Redo | 98.5 ms | 88.6 ms | 10.1% |
| Source to Live Preview | 5100.5 ms | 91.6 ms | 98.2% |
| First reading Preview | 2555.4 ms | 2498.6 ms | 2.2% |

Both runs verified the complete source after open, append, Undo/Redo, mode
changes and reopening. Reading Preview still spends about 2.5 seconds before its
first paint in this probe; eliminating that remaining cost is outside the query
compilation/Live Preview improvement measured here. Background syntax readiness
is not the endpoint of this measurement. Raw runs are
`target/markdown-performance/before-sfw560f4` and
`target/markdown-performance/after-b05bktfl`.
