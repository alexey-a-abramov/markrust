// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CI perf gates: load + parse of a large fixture must stay under a budget.

use std::time::{Duration, Instant};

use markrust_core::perf_fixture::{large_markdown, LARGE_MARKDOWN_TARGET_BYTES};
use markrust_core::rich::{import_markdown, IdGen};
use markrust_core::{extract_syntax_spans, Document};

/// CI-tolerant ceiling for generating the fixture, extracting source spans,
/// and importing a RichTree. Local debug is typically well under 300ms.
const LOAD_PARSE_BUDGET: Duration = Duration::from_millis(2500);

/// Background worker must finish the same document without hanging the caller.
const WORKER_PARSE_BUDGET: Duration = Duration::from_secs(3);

#[test]
fn large_document_load_and_parse_stays_under_budget() {
    let t0 = Instant::now();
    let source = large_markdown();
    assert!(
        source.len() >= LARGE_MARKDOWN_TARGET_BYTES,
        "fixture too small: {}",
        source.len()
    );
    let spans = extract_syntax_spans(&source);
    let tree = import_markdown(&source, &mut IdGen::default());
    let elapsed = t0.elapsed();
    assert!(
        !spans.is_empty(),
        "expected syntax spans for the load-test fixture"
    );
    assert!(
        tree.blocks.len() > 50,
        "expected a large block list, got {}",
        tree.blocks.len()
    );
    assert!(
        elapsed <= LOAD_PARSE_BUDGET,
        "load+parse of {} bytes took {elapsed:?} (budget {LOAD_PARSE_BUDGET:?})",
        source.len()
    );
}

#[test]
fn large_document_background_parse_stays_under_budget() {
    let source = large_markdown();
    let t0 = Instant::now();
    let mut doc = Document::new(&source);
    assert!(
        doc.syntax_spans.is_empty(),
        "Document::new must not parse on the caller"
    );
    assert!(
        doc.wait_for_parse(WORKER_PARSE_BUDGET),
        "background parse did not finish within {WORKER_PARSE_BUDGET:?}"
    );
    let elapsed = t0.elapsed();
    assert!(
        !doc.syntax_spans.is_empty(),
        "background parse produced no spans"
    );
    assert!(
        elapsed <= WORKER_PARSE_BUDGET,
        "background load+parse took {elapsed:?} (budget {WORKER_PARSE_BUDGET:?})"
    );
}
