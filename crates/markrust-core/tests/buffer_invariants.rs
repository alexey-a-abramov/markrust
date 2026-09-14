// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic generated edits for the byte-offset foundation shared by the
//! source and WYSIWYG editors. This is intentionally dependency-free so it
//! runs in the normal test suite; a future cargo-fuzz target can reuse the
//! same invariants with arbitrary input.

use markrust_core::{DocumentBuffer, LineIndex};

const INSERTIONS: &[&str] = &["", "x", "\n", "é", "你好", "🦀", "\r\n", "**"];

/// Tiny deterministic PRNG: reproducible failures are more useful in CI than
/// random test runs, while still exploring many edit orderings.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 32) as usize
    }
}

fn byte_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<_> = text.char_indices().map(|(offset, _)| offset).collect();
    boundaries.push(text.len());
    boundaries
}

fn check_invariants(buffer: &DocumentBuffer, expected: &str, seed: u64, step: usize) {
    assert_eq!(
        buffer.content(),
        expected,
        "buffer diverged from string model (seed {seed}, step {step})"
    );
    assert_eq!(buffer.len_bytes(), expected.len());

    let rebuilt = LineIndex::from_rope(buffer.text());
    assert_eq!(
        buffer.line_index().line_starts(),
        rebuilt.line_starts(),
        "incremental line index diverged from a rebuild (seed {seed}, step {step}, text {expected:?})"
    );

    // LineIndex deliberately uses byte columns, including interior UTF-8
    // bytes, because all editor carets and syntax ranges are byte offsets.
    for offset in 0..=buffer.len_bytes() {
        let (line, column) = buffer.line_col_of_offset(offset);
        assert_eq!(
            buffer.offset_of_line_col(line, column),
            offset,
            "line/column round trip failed at byte {offset} (seed {seed}, step {step})"
        );
    }
}

#[test]
fn generated_utf8_edits_keep_rope_and_line_index_in_sync() {
    for seed in 0..64 {
        let mut rng = Lcg::new(seed);
        let mut expected = String::from("# start\nαβ\n🦀\r\n");
        let mut buffer = DocumentBuffer::with_text(&expected);
        let mut expected_revision = 0u64;

        for step in 0..200 {
            let boundaries = byte_boundaries(&expected);
            let start = boundaries[rng.next() % boundaries.len()];
            let end = boundaries[rng.next() % boundaries.len()];
            let (start, end) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            let insertion = INSERTIONS[rng.next() % INSERTIONS.len()];

            match rng.next() % 3 {
                0 => {
                    buffer.insert(start, insertion);
                    expected.insert_str(start, insertion);
                    // DocumentBuffer records an explicit insert, including an
                    // empty one, as a revision so parser invalidation remains
                    // monotonic.
                    expected_revision += 1;
                }
                1 => {
                    buffer.delete(start, end);
                    if start < end {
                        expected.replace_range(start..end, "");
                        expected_revision += 1;
                    }
                }
                _ => {
                    buffer.replace(start, end, insertion);
                    if start < end {
                        expected.replace_range(start..end, "");
                        expected_revision += 1;
                    }
                    expected.insert_str(start, insertion);
                    expected_revision += 1;
                }
            }

            assert_eq!(
                buffer.revision(),
                expected_revision,
                "revision mismatch (seed {seed}, step {step})"
            );
            check_invariants(&buffer, &expected, seed, step);
        }
    }
}
