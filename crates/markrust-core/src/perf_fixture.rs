// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic large Markdown used by parse/layout budget tests and benches.

use std::fmt::Write as _;

/// Target size for the default load-test document.
pub const LARGE_MARKDOWN_TARGET_BYTES: usize = 256 * 1024;

/// Target size for source-mode layout budgets (debug layout is heavier than parse).
pub const LAYOUT_MARKDOWN_TARGET_BYTES: usize = 64 * 1024;

/// Build a ~256 KiB mixed GFM document (headings, lists, tables, fences, quotes).
pub fn large_markdown() -> String {
    generate(LARGE_MARKDOWN_TARGET_BYTES, true)
}

/// Build a ~64 KiB document without tree-sitter language fences, so the layout
/// gate measures masking/layout rather than highlighter warmup.
pub fn layout_markdown() -> String {
    generate(LAYOUT_MARKDOWN_TARGET_BYTES, false)
}

/// Build mixed GFM until `target_bytes` is reached.
pub fn large_markdown_with_target(target_bytes: usize) -> String {
    generate(target_bytes, true)
}

fn generate(target_bytes: usize, highlighted_fences: bool) -> String {
    let mut out = String::with_capacity(target_bytes + 2048);
    out.push_str(
        "---\ntitle: Perf Fixture\ndescription: Load-test document\ntags: [perf, gate]\n---\n\n",
    );
    let mut i = 0u32;
    while out.len() < target_bytes {
        i += 1;
        match i % 5 {
            0 => {
                let _ = writeln!(out, "## Heading {i}\n");
                let _ = writeln!(
                    out,
                    "Paragraph {i} with **bold**, *italic*, `code`, ~~strike~~, and a [link](https://example.com/{i}). More words so source-mode wrapping and span collection have real line length.\n"
                );
            }
            1 => {
                let _ = writeln!(
                    out,
                    "- item {i}-a with **mark**\n- item {i}-b\n- [ ] task {i}\n"
                );
            }
            2 => {
                let _ = writeln!(
                    out,
                    "| Col A | Col B | Col C |\n| --- | ---: | :---: |\n| {i} | x | y |\n| a | b | c |\n"
                );
            }
            3 => {
                if highlighted_fences && i % 20 == 3 {
                    let _ = writeln!(
                        out,
                        "```rust\nfn f_{i}() {{\n    let x = {i};\n    println!(\"{{x}}\");\n}}\n```\n"
                    );
                } else {
                    let _ = writeln!(out, "```\nplain fence {i}\nline two\n```\n");
                }
            }
            _ => {
                let _ = writeln!(
                    out,
                    "> quote {i} with *em*\n\nPlain paragraph filler {i} lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore.\n"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fixture_meets_target_and_has_structure() {
        let src = large_markdown();
        assert!(src.len() >= LARGE_MARKDOWN_TARGET_BYTES);
        assert!(src.contains("title: Perf Fixture"));
        assert!(src.contains("## Heading"));
        assert!(src.contains("| Col A |"));
        assert!(src.contains("```rust"));
        let layout = layout_markdown();
        assert!(layout.len() >= LAYOUT_MARKDOWN_TARGET_BYTES);
        assert!(!layout.contains("```rust"));
    }
}
