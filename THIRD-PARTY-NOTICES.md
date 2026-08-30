# Third-Party Notices

This repository redistributes test fixtures derived from third-party projects.
The files under `crates/markrust-core/tests/fixtures/` are used solely as golden
test corpora for the markdown round-trip test suite and are not part of the
shipped product.

## TOAST UI Editor

- Source: <https://github.com/nhn/tui.editor>
- License: MIT License
- Copyright (c) NHN Cloud Corp.

Derived fixtures:

- `crates/markrust-core/tests/fixtures/commonmark/base-examples.json` — copied
  verbatim from `libs/toastmark/src/commonmark/__test__/base-examples.json`.
- `crates/markrust-core/tests/fixtures/roundtrip/tui.txt` — test case strings
  extracted from `apps/editor/src/__test__/unit/convertor.spec.ts`.

### License notice (TOAST UI Editor)

```
MIT License

Copyright (c) 2020 NHN Cloud Corp.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Nimbalyst

- Source: <https://github.com/Nimbalyst/nimbalyst>
- License: MIT License
- Copyright (c) Nimbalyst Inc.

Derived fixtures:

- `crates/markrust-core/tests/fixtures/roundtrip/nimbalyst.txt` — test case
  strings extracted from
  `packages/runtime/src/editor/markdown/__tests__/round-trip-corpus.test.ts`,
  `checkbox-roundtrip.test.ts`, `blank-lines-regression.test.ts`, and the
  escaping example documented in
  `packages/runtime/src/editor/markdown/FORKED_MARKDOWN_IMPORT.md`.

### License notice (Nimbalyst)

```
MIT License

Copyright (c) 2024-2026 Nimbalyst Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## CommonMark specification examples

The examples in
`crates/markrust-core/tests/fixtures/commonmark/base-examples.json` originate
from the CommonMark specification by John MacFarlane
(<https://spec.commonmark.org/>), which is licensed under the Creative Commons
Attribution-ShareAlike 4.0 International License (CC-BY-SA 4.0,
<https://creativecommons.org/licenses/by-sa/4.0/>). The file is redistributed
here in the JSON form shipped by TOAST UI Editor's toastmark test suite.
