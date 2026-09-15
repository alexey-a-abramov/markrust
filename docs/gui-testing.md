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

When a visual bug is reported, add a small Markdown fixture that reproduces
it. Preserve the problematic structure and line lengths; remove unrelated
personal content. Confirm that its assertion fails with the old behavior
before treating the test as a regression guard.

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

## Related

- [Engineering documentation](README.md)
- [Contributing](../CONTRIBUTING.md)
- [Project roadmap](../ROADMAP.md)
