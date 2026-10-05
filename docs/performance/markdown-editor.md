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

## Markdown analysis during editing

The Live Preview provider no longer calls `wiki::parse`, `to_mdast`, or
`MarkdownIndex::reparse` from `display`/`prepaint`. Its optional `prepare` hook
schedules a single worker at a time. Each worker reads the newest requested Rope
snapshot and either reparses the bounded incremental window or performs the safe
full fallback. Edits arriving during a parse are combined relative to the last
accepted index; the next worker processes the newest snapshot, rather than one
job for every queued keystroke.

Global reference/footnote definitions still require full analysis for correctness.
The stress note has a reference definition near its end, so even unrelated tail
edits previously entered the synchronous full fallback. That CPU work now runs
on the background executor; this change does not claim incremental parsing of
all global dependencies. An already running Markdown parser cannot be interrupted.
The queue holds one active snapshot and the latest request, with no worker per key.

The authoritative editable source stays in `EditorState`. Read-only parser
snapshots do not write the document, selection, history, or composition state.
Foreground source edits shift unaffected token/block/task/link ranges and retain
cached geometry. Dirty tokens and ordinary rendered blocks show their actual
source until the accepted analysis arrives. Fenced-code content can be updated
lexically while keeping the nested editor's stable identity. Unknown replacements
and edits made in Source mode are compared as Rope characters, without copying
the entire document to a UI-thread string.

Publication checks the provider revision, current provider Rope, authoritative
editor Rope, and weak presentation/editor lifetimes. Old results cannot publish
after rapid edits or a document replacement. A completed result waits until drag
selection is released, keeping the pointer's presentation stable. UI publication
converts the accepted AST to presentation metadata and reuses unaffected caches;
this conversion and layout remain UI work, but Markdown parsing does not.

`EditorDisplayProvider::prepare(&Rope, &mut App)` is an additive, default no-op
hook. Existing provider implementations and NeverWrite APIs need no changes.

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

One fresh-process run per revision, `85ed12f1` versus `abf45895`, on the same
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

### Second change: separate keyboard events

An additional pair of isolated component runs compares `abf45895` with the
background Markdown analysis change. Both use the same verified stress-note
copy, binary harness, repository dev profile, ZUI revision, and X11/Xvfb setup.
The probe waits two seconds after opening before typing and sends the same
24-character marker through `xdotool type` with a 15 ms inter-event delay. It
then waits for a source snapshot containing all characters. Source snapshots,
Undo/Redo, Home/End, document replacement, and mode transitions passed in both
runs; the rendered open screenshot was also inspected.

| Operation | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| 24 separate keyboard events to verified source snapshot | 2320.6 ms | 391.7 ms | 83.1% |

These are single exploratory runs. The numbers include input pacing and snapshot
polling and exclude NeverWrite autosave and completion of all background styling.
They measure input responsiveness, not total Markdown CPU time. Initial Live
Preview can paint source before parsing completes, so its first-frame time is
not equivalent to a fully formatted document. The first reading Preview still
took about 2.6 seconds in the new run and remains a separate cost.

Artifacts: `target/markdown-performance/after-x48lko0_/result.json` and
`target/markdown-performance/after2-di2iyg03/result.json`, with the shared local
`probe-typing.py`. They are ignored local artifacts, not repository fixtures.
NeverWrite integration of the first change independently reported 18.48 s to
2.458 s for opening, but 12.017 s for typing plus autosave. Integration of this
second change must measure that remaining operation using NW's original profile;
component results must not be substituted for it.

Eight additional tests cover bounded/coalesced jobs and obsolete results under
rapid edits on a document with 64 fences/tables and a global definition, source
replacement while in Source mode, partial table/fence/Unicode edits against a
fresh full analysis, retained block caches and code identities, composition with
Undo/Redo, retired presentations, and publication held until drag release.
Final library results: base 361 passed / 1 existing ignored; component 469 passed.
The component also builds with `--no-default-features`.
