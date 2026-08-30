// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use markrust_core::extract_syntax_spans;
use markrust_core::perf_fixture::layout_markdown;
use markrust_editor::{build_display_layout, Caret, EditorTheme};

fn benches(c: &mut Criterion) {
    let source = layout_markdown();
    let spans = extract_syntax_spans(&source);
    let theme = EditorTheme::dark();
    let carets = [Caret::new(0)];
    let mut group = c.benchmark_group("large_markdown");
    group.sample_size(10);
    group.bench_function("build_display_layout", |b| {
        b.iter(|| {
            build_display_layout(
                black_box(&source),
                black_box(&spans),
                black_box(&carets),
                black_box(&[]),
                black_box(&theme),
            )
        })
    });
    group.finish();
}

criterion_group!(layout, benches);
criterion_main!(layout);
