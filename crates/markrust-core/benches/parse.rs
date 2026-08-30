// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use markrust_core::extract_syntax_spans;
use markrust_core::perf_fixture::large_markdown;
use markrust_core::rich::{import_markdown, IdGen};

fn benches(c: &mut Criterion) {
    let source = large_markdown();
    let mut group = c.benchmark_group("large_markdown");
    group.sample_size(10);
    group.bench_function("extract_syntax_spans", |b| {
        b.iter(|| extract_syntax_spans(black_box(&source)))
    });
    group.bench_function("import_markdown", |b| {
        b.iter(|| import_markdown(black_box(&source), black_box(&mut IdGen::default())))
    });
    group.finish();
}

criterion_group!(parse, benches);
criterion_main!(parse);
