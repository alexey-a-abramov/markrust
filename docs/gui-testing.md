# Native GUI regression testing

Markdown round trips and editing unit tests cannot detect text painting over
the next list item or table row. MarkRust therefore tests the native GPUI
layout and input path as well as the document model.

## Run on macOS

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

The default run uses 32 scenarios per theme, combining curated
journeys with deterministic generated action sequences. For a longer local
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

For a visual action-by-action review, capture every frame explicitly:

```bash
cargo run --locked -p markrust-app --features gui-tests --example gui_regression -- --filter paragraph --record-frames --usecases-count 20 --output target/gui-review
```

Open a generated `.review.html` in a browser. Its timeline joins the screenshot,
source change, selection direction, input owner, context and response delta.
It has no network dependencies. `--record-frames` needs Metal and cannot be
combined with `--geometry-only`; ordinary CI still writes the state timeline.

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
- Split shows literal source; WYSIWYG follows the chosen markup-hint policy.
- Navigation, focus, mode and hint changes do not mutate Markdown bytes.
- Mode and hint changes preserve selection and caret position.
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

The JSON schema is versioned (`ui.schema_version`). `response` records which
document, selection, focus, mode and viewport properties changed. This is the
small, inspectable in-memory logical model used by the test runner; there is
no always-on recorder, telemetry or user-content logging in the shipped app.

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
