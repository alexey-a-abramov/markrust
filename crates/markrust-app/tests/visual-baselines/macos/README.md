# macOS visual baselines

Initial visual review: 2026-09-15. The 28 references cover the reported wrapping
and selection defects, editing modes, themes, and compact panel layouts.

Environment for the initial baseline set:

- macOS 26.6 (25G72), Apple Silicon (`arm64`).
- Xcode 26.6 (17F113), native CoreText shaping and Metal rendering.
- GPUI revision `8166e3d7b8b42d8aaf4d4dee7fcd25ab4ec65105`.
- Bundled Inter at 16pt, system Menlo for code, fixed 2× backing scale.
- Light and dark themes; 720pt and 1200pt window widths, 1040pt height.

These are offscreen application-content captures, not screenshots of the macOS
menu bar or native file dialogs. Compare on a matching environment; do not
automatically replace references when the operating system or fonts change.

Source mode keeps physical lines unwrapped. Its captures start at horizontal
offset zero; the interaction checks separately prove that long lines can be
scrolled and that the caret remains reachable. Floating panels intentionally
cover a pane at compact widths without changing the document's layout width.

See the [GUI testing guide](../../../../../docs/gui-testing.md) for the explicit
review, update, and comparison commands. CI runs geometry and input assertions;
it does not update or compare these Metal images.
