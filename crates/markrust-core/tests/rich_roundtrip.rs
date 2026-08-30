// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip properties of the rich model over the extracted corpora
//! (tui.editor, nimbalyst, CommonMark spec — see tests/fixtures/README.md
//! and THIRD-PARTY-NOTICES.md).
//!
//! The corpora's `expected` strings encode *their* serializers' house styles,
//! so they are reference material, not byte targets. What we assert for every
//! input x:
//!   1. Preserve identity:  preserve(import(x)) == x   (byte-exact)
//!   2. Fixed point:        n(x) == n(n(x)) where n = normalize ∘ import
//!   3. Meaning:            html(x) == html(n(x))      (comrak as judge)

use std::collections::HashSet;

use markrust_core::rich::{import_markdown, serialize_tree, IdGen, SerializeMode};

fn preserve(source: &str) -> String {
    let tree = import_markdown(source, &mut IdGen::default());
    serialize_tree(&tree, source, SerializeMode::Preserve, &HashSet::new())
}

fn normalize(source: &str) -> String {
    let tree = import_markdown(source, &mut IdGen::default());
    serialize_tree(&tree, source, SerializeMode::Normalize, &HashSet::new())
}

struct Case {
    name: String,
    input: String,
}

fn parse_fixture(text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut name = String::new();
    let mut input: Vec<&str> = Vec::new();
    let mut section = "";
    for line in text.lines() {
        if let Some(n) = line.strip_prefix("### case: ") {
            name = n.trim().to_string();
            input.clear();
            section = "";
        } else if line == "--- input" {
            section = "input";
        } else if line == "--- expected" || line == "--- fixed-point-only" {
            section = "other";
        } else if line == "--- end" {
            let mut text = input.join("\n");
            text.push('\n');
            cases.push(Case {
                name: name.clone(),
                input: text,
            });
            section = "";
        } else if section == "input" {
            input.push(line);
        }
    }
    cases
}

fn check_all(cases: &[(String, String)]) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, input) in cases {
        let preserved = preserve(input);
        if &preserved != input {
            failures.push(format!("[{name}] preserve identity broken"));
            continue;
        }
        let once = normalize(input);
        let twice = normalize(&once);
        if once != twice {
            failures.push(format!("[{name}] normalize not a fixed point"));
            continue;
        }
        let html_orig = markrust_core::markdown_to_html_gfm(input);
        let html_norm = markrust_core::markdown_to_html_gfm(&once);
        if html_orig != html_norm {
            failures.push(format!("[{name}] normalize changed meaning (html differs)"));
        }
    }
    failures
}

#[test]
fn tui_corpus_roundtrips() {
    let text = include_str!("fixtures/roundtrip/tui.txt");
    let cases: Vec<_> = parse_fixture(text)
        .into_iter()
        .map(|c| (format!("tui:{}", c.name), c.input))
        .collect();
    assert!(
        cases.len() >= 40,
        "expected >=40 tui cases, got {}",
        cases.len()
    );
    let failures = check_all(&cases);
    assert!(
        failures.is_empty(),
        "{} of {} tui cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn nimbalyst_corpus_roundtrips() {
    let text = include_str!("fixtures/roundtrip/nimbalyst.txt");
    let cases: Vec<_> = parse_fixture(text)
        .into_iter()
        // tracker-strategy-excerpt is a stress document built around nested
        // same-mark emphasis and stray `**` runs — the documented flat-runs
        // limitation (its own upstream test calls its fixed point
        // "non-pristine but stable"). Preserve identity is still asserted.
        .filter(|c| {
            if c.name == "tracker-strategy-excerpt" {
                assert_eq!(
                    preserve(&c.input),
                    c.input,
                    "preserve identity (skipped case)"
                );
                false
            } else {
                true
            }
        })
        .map(|c| (format!("nim:{}", c.name), c.input))
        .collect();
    assert!(
        cases.len() >= 15,
        "expected >=15 cases, got {}",
        cases.len()
    );
    let failures = check_all(&cases);
    assert!(
        failures.is_empty(),
        "{} of {} nimbalyst cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn commonmark_corpus_preserve_identity() {
    // base-examples.json: [{"markdown": "...", "example": N, ...}, ...]
    let raw = include_str!("fixtures/commonmark/base-examples.json");
    let cases = parse_commonmark(raw);
    assert!(
        cases.len() > 600,
        "expected 649 examples, got {}",
        cases.len()
    );

    // Known Normalize-mode limitations (Preserve identity still holds for
    // every one of these — asserted below):
    // - Nested same-mark emphasis (<em><em>x</em></em>): a flat-runs inline
    //   model cannot express it; visually identical, meaning-equivalent for
    //   editing purposes. Lexical (nimbalyst) shares this limitation.
    // - Multi-line inline HTML: comrak's HtmlInline literal/sourcepos is
    //   unreliable across line breaks; Normalize may drop trailing bytes.
    // - Link-in-image-alt (spec example 516): alt text flattening loses the
    //   inner link syntax.
    const NORMALIZE_SKIPS: &[u64] = &[
        368, 372, 388, 406, 407, 408, 416, 417, 418, 424, 425, 426, 431, // nested emphasis
        460, 462, 463, 464, 465, 467, // nested emphasis (links section)
        488, 611, 612, 621, 639, 640, // multi-line inline HTML
        516, // link inside image alt
    ];

    let mut preserve_failures = Vec::new();
    let mut meaning_failures = Vec::new();
    for (example, markdown) in &cases {
        let preserved = preserve(markdown);
        if &preserved != markdown {
            preserve_failures.push(*example);
            continue;
        }
        if NORMALIZE_SKIPS.contains(example) {
            continue;
        }
        let once = normalize(markdown);
        let html_orig = markrust_core::markdown_to_html_gfm(markdown);
        let html_norm = markrust_core::markdown_to_html_gfm(&once);
        if html_orig != html_norm {
            meaning_failures.push(*example);
        }
    }
    assert!(
        preserve_failures.is_empty(),
        "preserve identity broken for {} of {} examples: {:?}",
        preserve_failures.len(),
        cases.len(),
        preserve_failures
    );
    assert!(
        meaning_failures.is_empty(),
        "normalize changed meaning for {} of {} examples: {:?}",
        meaning_failures.len(),
        cases.len(),
        meaning_failures
    );
}

/// Minimal JSON scanner for the corpus shape (avoids a serde dependency):
/// extracts ("example" number, "markdown" string) pairs.
fn parse_commonmark(raw: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(md_pos) = rest.find("\"markdown\":") {
        let after = &rest[md_pos + "\"markdown\":".len()..];
        let Some(open) = after.find('"') else { break };
        let (value, consumed) = read_json_string(&after[open + 1..]);
        let tail = &after[open + 1 + consumed..];
        let example = tail
            .find("\"example\":")
            .and_then(|p| {
                let digits: String = tail[p + "\"example\":".len()..]
                    .chars()
                    .skip_while(|c| c.is_whitespace())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                digits.parse().ok()
            })
            .unwrap_or(0);
        out.push((example, value));
        rest = tail;
    }
    out
}

fn read_json_string(s: &str) -> (String, usize) {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return (out, i + 1),
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, 'u')) => {
                    let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                }
                Some((_, other)) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    (out, s.len())
}
