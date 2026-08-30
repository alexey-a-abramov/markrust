// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CI perf gate: source-mode layout of a large fixture must stay under a budget.

use std::time::{Duration, Instant};

use markrust_core::extract_syntax_spans;
use markrust_core::perf_fixture::{layout_markdown, LAYOUT_MARKDOWN_TARGET_BYTES};
use markrust_editor::{build_display_layout, Caret, EditorTheme};

/// Layout includes delimiter masking and table padding. CI-tolerant; local
/// debug of the 64 KiB layout fixture is typically well under 500ms.
const LAYOUT_BUDGET: Duration = Duration::from_millis(2500);

#[test]
fn large_document_layout_stays_under_budget() {
    let source = layout_markdown();
    assert!(
        source.len() >= LAYOUT_MARKDOWN_TARGET_BYTES,
        "layout fixture too small: {}",
        source.len()
    );
    let spans = extract_syntax_spans(&source);
    let theme = EditorTheme::dark();
    let carets = [Caret::new(0)];
    let t0 = Instant::now();
    let layout = build_display_layout(&source, &spans, &carets, &[], &theme);
    let elapsed = t0.elapsed();
    assert!(
        !layout.display_text.is_empty(),
        "layout produced empty display text"
    );
    assert!(
        elapsed <= LAYOUT_BUDGET,
        "build_display_layout on {} bytes / {} spans took {elapsed:?} (budget {LAYOUT_BUDGET:?})",
        source.len(),
        spans.len()
    );
}
