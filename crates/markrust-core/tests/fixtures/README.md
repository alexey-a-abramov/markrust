# Test fixtures

Golden corpora for the markdown round-trip test suite (see `THIRD-PARTY-NOTICES.md`
at the repo root for provenance and licenses). `commonmark/base-examples.json` is a
verbatim copy of the CommonMark spec examples as shipped by TOAST UI Editor's
toastmark (`markdown`/`html` pairs). `roundtrip/tui.txt` holds the markdown → WYSIWYG →
markdown conversion cases extracted from tui.editor's `convertor.spec.ts`, with the
`source`/`oneLineTrim` template dedenting resolved to the exact strings the tests
assert. `roundtrip/nimbalyst.txt` holds the round-trip cases from Nimbalyst's
`round-trip-corpus.test.ts`, the checkbox and blank-line regression inputs from
`checkbox-roundtrip.test.ts` / `blank-lines-regression.test.ts`, and the
`***\*syntax\****` literal-asterisk escaping example from its
`FORKED_MARKDOWN_IMPORT.md`.

## Fixture text format

```
### case: <kebab-name>
--- input
<input markdown>
--- expected
<expected markdown>
[--- fixed-point-only]
--- end
```

Marker lines (`### case: `, `--- input`, `--- expected`, `--- fixed-point-only`,
`--- end`) are exact full lines. A block's value is the byte region between its
marker line and the next marker line, minus one trailing `\n`; an empty region
(markers on adjacent lines) is the empty string, and a value that itself ends
with a newline is followed by a blank line before the next marker. `--- fixed-point-only`
means no exact expected string exists in the source test: the expected block is a
copy of the input, and only round-trip stability should be asserted
(export(import(x)) must be a fixed point), not byte equality with the input.
Notes: the `front-matter` case in `tui.txt` requires front-matter parsing enabled;
`table-with-unmatched-html-list` relies on HTML tag auto-balancing; the
`frontmatter-*`/`checkbox-*` cases in `nimbalyst.txt` were asserted with
frontmatter emission enabled in the source suite.

## Cases skipped from convertor.spec.ts

Skipped because they depend on runtime editor features (custom convertor plugins,
javascript:/vbscript: URL sanitization, custom HTML renderers) or have no markdown
input, which makes no sense for a parser corpus:

- `href attribute with link` (sanitizer)
- `src attribute with image` (sanitizer)
- `should change delimeter` (custom to-md convertor)
- `should change raw html` (custom to-md convertor)
- `should not convert raw html when returning only delimiter` (custom to-md convertor)
- `should convert to original value` (custom to-md convertor)
- `should convert by mixing return values` (custom to-md convertor, setext headings)
- `should convert html block node to wysiwyg ignoring sanitizer tag` (custom HTML renderer)
- `should convert html block element which has "=" character as the attribute value` (custom HTML renderer)
- `should convert html block node as the block node through inserting the blank line` (custom HTML renderer)
- `should convert html inline node` (custom HTML renderer)
- `should convert markdown to wysiwyg` (custom md-to-wysiwyg convertor)
- `should convert empty line between lists of wysiwig to <br>` (built from a WYSIWYG node JSON, no markdown input)
