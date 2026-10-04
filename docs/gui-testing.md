# Native GUI regression testing

Markdown round trips and editing unit tests cannot detect text painting over
the next list item or table row. MarkRust therefore tests the native GPUI
layout and input path as well as the document model.

## Run on macOS

Split-view observations also expose a separate `shadow` on the inactive pane:
source revision, ordered range, direction, active edge, and independently
observed cursor/selection scene quads. A shadow is never the input selection.
Contracts check owner agreement, mapped glyph coverage, unchanged peer input
state and scroll, blink independence, and dismissal for non-body focus, an open
image inspector or standalone modes. Both directions and reversed Unicode
selections have native journeys. Hidden Markdown syntax maps to visible text
edges; image/widget-only ranges may have no text geometry. Unpainted offscreen
context does not force either pane to scroll.

The `image_inspector` observation records its private fixture fields, approved
preview state, actual focused field and painted panel bounds. Its input owner
is distinct from the body caret, including after tab return. Inspector checks
use temporary raster files and real native text fields; they do not read user
documents or authorize remote content. Metal checks require fixture raster
pixels inside the actual preview bounds, not merely an approved cache entry.
Compact-window wheel checks require the inspector content to move while its
action buttons and the underlying document remain fixed. List journeys
separately record the first paints after a bare hyphen, autoformat
cancellation, and Enter/Enter/click/typing so a correct final Markdown string
cannot conceal an earlier missing caret or accidental bullet.

Use full Xcode with the Metal toolchain selected. Check `xcrun --find metal`
before building. From the repository root:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --output target/gui-regression
```

The runner uses GPUI's `HeadlessAppContext`, native macOS text shaping, and
Metal rendering. It runs on the main thread, uses fixture documents, and
renders directly to images. It does not need Screen Recording permission or
open a window on the desktop. The normal editor binary does not include this
test harness.

The default run combines screenshot fixtures, curated
journeys, and deterministic generated action sequences. For a longer local
stress run, pass `--usecases-count 500`; the CI gate keeps the bounded default
so failures produce evidence promptly.

The use-case runner records each fixture action and the resulting editor model
in `<output>/<theme>/journeys/usecases/`. Each journey gets a `.jsonl` trace,
machine-readable `.report.json`, and an offline `.review.html` timeline. A
failure keeps the first failing frame and, when Metal is available, its
`.failure.png`. Light and dark evidence do not overwrite one another. Journeys
declare their own document templates; they are not repeated for each screenshot
fixture. `--filter` limits screenshot fixtures, not the action matrix.
These are test-fixture artifacts, not production document or keystroke logs.

For a quick local check while working on navigation:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --geometry-only --filter paragraph --usecases-count 12 --output target/gui-regression-smoke
```

To isolate a reported input journey while iterating:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --geometry-only --journeys mouse_ --output target/gui-mouse-smoke
```

`--journeys` matches a curated scenario-name substring, runs both themes and
explicitly skips screenshot goldens and lifecycle checks. It is not the complete
GUI gate. Remove `--geometry-only` and add `--record-frames` for visual evidence.

For isolated concurrent-edit and long normalization-review fixtures:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --concurrent-only --output target/gui-concurrent-review
```

Add `--geometry-only` for CI without Metal. This subset writes diagnostic PNGs
and logical/bounds JSON, not golden baselines; the complete default run includes
it alongside the existing editing journeys.

For isolated window routing and private-draft lifecycle fixtures:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --notepad-only --output target/gui-notepad
```

This diagnostic subset cannot update golden baselines or be combined with
`--concurrent-only` / journey frame recording. The full run includes both.

For isolated image-inspector input, preview and private-draft checks:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --images-only --output target/gui-images
```

This subset checks both themes and writes diagnostic screenshots and state
traces. It cannot compare/update goldens or be combined with other subsets.
The complete default run includes these checks too.

Document Find is an input owner separate from both editing panes. Its observations
record query selection, literal source ranges, active match, originating pane and
native input registration. Contracts independently join actual highlight quads
to native glyph rows, including hidden syntax, reversed Unicode selections,
tab/mode return, no-results dismissal and deep-document visible-match return.
Query typing must not mutate body bytes, revision, history or real selection;
navigation may reveal only the originating pane, never the passive Split peer.

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --journeys find_ --record-frames --output target/gui-find
```

Open Location and live-language checks use synthetic files and nonpersistent
workspace configuration. They check native path-field focus, inline errors,
Escape, dirty-tab preservation, file/folder routing, live query retention and
unchanged selections/viewport after locale changes. Localized screenshots and
the production menu model do not certify OS menu activation, full RTL layout,
native-speaker translations, VoiceOver or candidate-window IME behavior.
Cross-platform distribution gates and their limitations are recorded in
[binary distribution](deployment.md#binary-distribution).

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --open-path-only --output target/gui-open-path
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --locale-only --output target/gui-locale
```

These flags can use `--geometry-only` but cannot read/update baselines, record
journey frames or replace the complete default gate. The default gate includes
both groups.

For a visual action-by-action review, capture every frame explicitly:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --filter paragraph --record-frames --usecases-count 20 --output target/gui-review
```

Open a generated `.review.html` in a browser. Its timeline joins the screenshot,
source change, selection direction, input owner, context and response delta.
It has no network dependencies. `--record-frames` needs Metal and cannot be
combined with `--geometry-only`; ordinary CI still writes the state timeline.
PNG frames use fast lossless encoding; decoded pixels and baseline comparison
are unchanged. This avoids adaptive-filter compression dominating debug runs.

Window-lifecycle and persistence fixtures use independent temporary recovery
stores. They must inspect the actual routing/window registry and document bytes:
last-active Finder routing, private checkpoints without source-file writes,
graceful close/restart, changed-disk reconciliation and failed-checkpoint veto.
They must never read or replace the user's production drafts. See
[everyday notepad](notepad-experience.md) for the lifecycle contract.

## State-to-render contracts

The observation layer reads the real `Document`, editor views, GPUI focus and
painted scene. It does not implement another parser, virtual DOM or editing
engine. Source-backed glyph caret stops connect Markdown byte ranges to native
text rows. Nested context identifies selected list items or table cells, not
just a top-level block; its revision is explicit when the rich pane is hidden.

Every curated and generated action checks:

- Input belongs to a visible pane; Split restores the last active pane per tab.
- Selection endpoints are valid UTF-8 boundaries and the caret matches the
  active edge, including reversed selections.
- Visible Source and rich layouts represent the current document revision.
- Source and Split show literal monospace Markdown; WYSIWYG follows the chosen
  markup-hint policy.
- Pointer scenarios derive positions from actual painted caret stops and dispatch
  native MouseDown, MouseMove and MouseUp events. Forward/reversed Cyrillic word
  drags, double-clicks, pane ownership and click-then-Enter behavior are checked
  independently of programmatic selection setup. A logical caret jump alone
  cannot prove mouse handlers are connected.
- Granular pointer journeys assert the press anchor, every delivered move and
  the release endpoint separately. The release may differ from the last move,
  or arrive without a move. Interparagraph padding must replace a stale anchor
  while painted widget and contextual-control surfaces retain their own input.
- Manually scrolled long-document journeys delete visible text in Source,
  WYSIWYG and both Split owners. They assert the viewport offset and actual row
  placement, including fully visible carets one pixel from either pane edge.
  A visible caret must not trigger comfort-margin scrolling.
- Navigation, focus, mode and hint changes do not mutate Markdown bytes.
- Mode and hint changes preserve selection and caret position.
- Hint and highlight-palette changes preserve shaped text, source mapping,
  viewport bounds, row coordinates and caret stops; caret-only context changes
  cannot resize the editor viewport.
- Empty bullet, ordered and task items own a visible insertion caret. List
  continuation and empty-item exit are checked immediately after Enter.
- Trailing-space input advances native caret geometry immediately, including
  Unicode text, table boundary entities and Undo/Redo. Arrow steps assert exact
  source stops, not merely nondecreasing offsets. First Enter moves to a new
  empty paragraph; repeated Enter there preserves bytes, revision and selection.
  The first letter retains the draft row's baseline.
- Deterministic idle actions inspect blink phase and actual caret-colored scene
  quads in Source, the WYSIWYG body and a focused URL draft. Input and Undo/Redo
  wake the caret; blink-off frames retain valid IME geometry. Widget probes use
  draft selection and bounds, not the body's source anchor. Fault-injection
  tests reject stationary, missing or incorrectly owned carets.
- Rich tables paint editable text without pipes, delimiter rows or persistent
  controls. The contextual panel is bounded, avoids the active row and disappears
  during selection, dragging or non-table editing; literal escaped pipes remain
  content, not structural chrome.
- Link URL fields retain a visible native caret even with hints disabled.
  Long destinations scroll inside the field; draft undo, commit and cancellation
  preserve rich text placement. Test-fixture traces distinguish draft text from
  document bytes and never record production user input.
- New-tab and tab-cycling shortcuts preserve each buffer and input owner;
  many-tab journeys verify the active tab's actual bounds inside its strip.
- Native recovery probes type through both panes, reopen isolated session
  stores, and verify dirty Unicode buffers, tab order, Split focus and the
  newer-disk publication guard and private checkpoints. Closed named drafts cannot become a second writable
  owner of the original file. They do not load the user's recovery files.
- Native URL-draft persistence probes verify Cmd-S, tab close, archive ordering
  and restart against actual isolated files; the saved/archived URL must match
  the visible field rather than its earlier document value.
- Command-palette probes require exclusive native input ownership, Unicode query
  selection and no query leakage into the document. Escape/outside-click restore
  the remembered pane; Enter executes the selected typed target. Duplicate tab
  titles resolve by identity, not label matching.
- Concurrent-edit probes exercise Save without watcher delivery, merge/conflict
  disk checks, stale review tokens, recoverable side selection and restart.
  The ordinary Save mode matrix includes dirty WYSIWYG, Source and Split buffers
  and asserts exact bytes and no normalization review.
  Long table normalization verifies native wheel/key scrolling, bounded modal
  geometry, fixed controls, actual visible diff rows and blocked editor input.
  These fixtures use isolated files and recovery stores, never user documents.
- Selection scene quads cover the expected shaped glyph rows, without missing,
  extra, stale, tall or incorrectly clipped highlights.
- Wrapped rows do not overlap, including rows within the same text leaf;
  opaque code backgrounds cannot paint over selection.

Each journey starts with a fresh document, undo history, WYSIWYG focus and
known hint policy. Checks run immediately after each settled action, and the
trace is flushed before an assertion. This prevents a later action from
concealing an earlier bad state. Generated journeys use the same contracts;
they are not merely crash smoke tests. Fault-injection unit tests verify that
the geometry/state oracles reject deliberately broken observations.

Viewport/selection journeys also inspect the first three native paints after
each action, before waiting for settled state. Their `.paints.jsonl` records
are flushed before each assertion; `--record-frames` additionally captures
`.step-N.paint-M.png` images. This catches a transient jump or selection loss
that a settled screenshot could conceal. Pointer positions are held across
the gesture: recomputing them from newly moved glyphs would hide a layout bug.

The JSON schema is versioned (`ui.schema_version`). `response` records which
document, selection, focus, mode and viewport properties changed. This is the
small, inspectable in-memory logical model used by the test runner; there is
no always-on recorder, telemetry or user-content logging in the shipped app.
Recovery screenshots remain diagnostic artifacts rather than golden images:
their warning labels contain per-run temporary paths. The same native geometry,
buffer, focus and filesystem assertions still run.
Isolated test workspaces do not start filesystem watchers: the deterministic
executor cannot host their indefinite blocking waits. Recovery tests compare
real temporary disk bytes directly; external-edit policy has separate unit tests.

## What belongs in this suite

- Long prose, links, inline code, nested lists, and narrow table cells.
- Different window widths, editing modes, and light and dark themes.
- Selection, Unicode typing, clipboard replacement, undo/redo, and mode changes
  through the editor's input path.
- Long unwrapped Source lines: horizontal scrolling and End/Home caret-follow
  in both Source and Split views.
- Geometry assertions: painted rows must fit their allocated height, avoid
  unrelated rows, and stay inside the editor's available width.
- Rendered screenshots for review and comparison with approved baselines.
- Source/Split partial multi-line selection, code selection layering,
  reversed Unicode selection, pane restoration and shared-document deletion.

When a visual bug is reported, add a small Markdown fixture that reproduces
it. Preserve the problematic structure and line lengths; remove unrelated
personal content. Confirm that its assertion fails with the old behavior
before treating the test as a regression guard.

For focus or scroll bugs, assert the logical response after each meaningful
action (active pane, selection, caret visibility, and viewport anchor), then
capture a small number of visual checkpoints. The editor's `Document` and
render state are the source of truth; tests should inspect that state rather
than maintain a second model that can drift from the UI.

## Compare screenshots

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --output target/gui-regression --baseline crates/markrust-app/tests/visual-baselines/macos
```

The test window uses a fixed 2× backing scale. Use the same macOS, fonts, and
renderer when comparing pixel baselines. Native text rasterization can change
between operating-system versions; geometry assertions remain the primary
portable gate. Missing baselines must fail comparison, never silently approve
the current image.

For an intentional UI change, inspect the generated screenshots first. Only
then regenerate the corresponding baselines with the runner's explicit
`--update-baselines` option:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --output target/gui-regression --baseline crates/markrust-app/tests/visual-baselines/macos --update-baselines
```

Review baseline image changes alongside the code; CI must never update them
automatically. Output and baseline directories must be different.
The [baseline environment](../crates/markrust-app/tests/visual-baselines/macos/README.md)
records the native renderer, fonts, display scale, and window sizes.

## Regression evidence

The original renderer at `05c7d20` failed the new table fixture at 720px:
it allocated 47px of cell height (y=269–316), then painted a wrapped row at
y=316.1–339.7. The test exits with `glyph rows overflow allocated leaf height`.
The comparison used an isolated source copy with the original `block_text.rs`
and `blocks.rs`, plus the current test instrumentation; the working source
was not rolled back.

The native input journey also caught an independent failure after typing a
trailing space: the parser's leaf ranges no longer included the caret and the
input handler disappeared. A body-level fallback now retains native input
registration while painted leaves provide precise caret geometry.

Clipboard replacement exposed another real editing defect: Undo removed pasted
text without restoring the selected text it replaced. Replacement and Paste now
form atomic undo groups, preserve reversed Unicode selections, and remain
separate from adjacent typing. The native test uses its own test-platform
clipboard, not the user's system clipboard.

The 0.4.0 pointer fixtures exposed input defects that programmatic selection
tests could not catch: Source mouse handlers existed but were not registered,
and every rich leaf could overwrite a drag's active endpoint. The terminal
heading-glyph click also reproduced an exact source-stop mismatch (`16` became
`17`) in GPUI's nearest-index helper. Hit testing now uses fresh painted caret
stops, including the terminal stop. The user's Cyrillic-word example is a
fixture in standalone and Split modes, with forward/reversed drag and
click-then-Enter assertions. Table actions inspect complete live `RichTree`
dimensions; selection-filtered context alone cannot certify all rows/columns.

The 0.5.0 granular fixtures reproduced an endpoint defect in both editors:
press at byte `129`, move to `131`, release at `137` selected only `129..131`.
A rich margin press also retained the old byte `2` instead of anchoring at
the painted Cyrillic paragraph's byte `76`. The failed first-paint traces are
retained under `target/gui-pointer-before-fix/`,
`target/gui-rich-pointer-before-fix/` and `target/gui-padding-before-fix/`.
These checks dispatch native input, not direct selection commands.

The viewport correction preserves measured block visibility before list
remeasurement invalidates its bounds. An already visible edited block is no
longer mistaken for an offscreen block and anchored at the viewport top.
Fresh caret geometry can still reveal a genuinely offscreen caret. This is
not a ban on rendering while typing: document layout and drag highlights
continue updating, without gratuitous scrolling.

The complete shell gate then caught a floating Outline press reaching the
document underneath it: navigation selected the heading, but MouseUp replaced
that position with byte `11`. Floating Files and Outline containers now own
their presses; palette and review overlays already had equivalent barriers.
The failed run remains in `target/gui-viewport-selection-final/`. A separate
Source probe caught clicks below the last painted row retaining its horizontal
column rather than reaching EOF; Source now resolves that empty area to EOF.

## Continuous integration

The macOS CI and release verification jobs run the same native layout and
interaction checks using:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --geometry-only --output target/gui-regression
```

This avoids requiring a GPU on GitHub's virtual Macs. It still uses native
text shaping and the real GPUI layout and input paths. It does **not** prove
pixel correctness; run the full screenshot comparison on a Metal-capable Mac
before accepting a visual change. CI uploads the regression evidence even
when an assertion fails.

## Native-window acceptance

The offscreen runner cannot prove AppKit menu behavior, VoiceOver navigation,
file dialogs, or operating-system IME candidate windows. Keep a short native
window pass for those features. XCTest/XCUIAutomation is the next layer for
automating operating-system interactions once the app has a packaged identity
and sufficient accessibility semantics. It complements the renderer tests.

The window design follows Apple's guidance on
[macOS conventions](https://developer.apple.com/design/human-interface-guidelines/designing-for-macos),
[toolbars](https://developer.apple.com/design/human-interface-guidelines/toolbars),
and [segmented controls](https://developer.apple.com/design/human-interface-guidelines/segmented-controls):
put commands in the menu bar, keep frequent toolbar actions compact, and give
each view-mode segment a consistent icon, tooltip, and visible selected state.
Focus and selection are distinct state in the observer, consistent with
[Apple's focus and selection guidance](https://developer.apple.com/design/human-interface-guidelines/focus-and-selection/).

## Related

- [Engineering documentation](README.md)
- [Contributing](../CONTRIBUTING.md)
- [Project roadmap](../ROADMAP.md)
- [Concurrent editing](concurrent-editing.md)
- [Everyday notepad](notepad-experience.md)
