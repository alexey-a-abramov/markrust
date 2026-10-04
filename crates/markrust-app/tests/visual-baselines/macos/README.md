# macOS visual baselines

Initial visual review: 2026-09-15. The 28 references cover the reported wrapping
and selection defects, editing modes, themes, and compact panel layouts.

Reviewed update: 2026-10-01. The current 32 references include editor-column
tabs, compact formatting overflow controls, native blue selection, and stable
paint-only WYSIWYG hints. Changes were inspected before explicit regeneration;
per-action link/recovery screenshots remain diagnostic artifacts, not goldens.

Reviewed update: 2026-10-02 for version 0.4.0. Source and Split now use literal,
uniform monospace Markdown with color-only highlighting. Rich editing retains
its layout and adds optional passive syntax badges outside shaped text. Native
pointer journeys cover Cyrillic word selection, terminal glyph clicks, reverse
and cross-paragraph drags; they record actual input ownership and painted caret
stops. Context-only table controls are verified through real button clicks and
full live-model table dimensions, not approved merely from a screenshot.

Reviewed update: 2026-10-04 for version 0.6.0. The 32 references retain their
scope and add literal H1/H2/H3/Body labels and passive Split cursor/selection.
The list fixture now preserves its authored blank separator before the following
paragraph. Unaffected text regions were pixel-compared before regeneration;
the H1 status change reflects its verified real button caret position. Image
inspector preview/body/compact screenshots remain diagnostics, not goldens.
Native inspector checks additionally require actual image/placeholder clicks,
focused field ownership and visible Apply/Cancel hit targets.

Reviewed update: 2026-10-04 for version 0.7.0. The existing 32-reference scope
is unchanged. Intentional differences are the consistent icon family,
full-path/unsaved header caption, clearer Open Files/No folder labels and
explicit unsaved status. All document regions were compared before approval;
only 1–2 emoji-edge pixels differed by one channel unit in some native captures,
with no line/caret/selection movement. Path errors, visible folder/outside-file
groups and RU/JA/AR/EN live-language states remain diagnostics, not goldens.
Find journeys additionally inspect actual source-addressed highlight painting,
native query focus, pane-scoped navigation and visible-match dismissal.

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
