// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich document model: structural projection of the markdown source for the
//! WYSIWYG editor. The source string stays the single source of truth; see
//! `docs/architecture.md`.

pub mod command;
pub mod engine;
pub mod escape;
pub mod import;
pub mod input_rules;
pub mod save;
pub mod serialize;
pub mod tree;

pub use command::{apply_rich_command, BlockType, CaretState, RichCommand, RichError, RichOutcome};
pub use engine::{Bias, BlockSpan, BlockSplice, RichEngine, TablePos};
pub use import::import_markdown;
pub use input_rules::{match_input_rule, InputRule};
pub use save::{save_candidates, DiffHunk, SaveCandidates};
pub use serialize::{serialize_block, serialize_tree, SerializeMode};
pub use tree::{
    Block, BlockKind, BreakStyle, ColumnAlign, FenceFidelity, Frontmatter, HeadingStyle, IdGen,
    Inline, LinkAttrs, MarkFidelity, MarkSet, NodeId, RichTree,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn import(source: &str) -> RichTree {
        import_markdown(source, &mut IdGen::default())
    }

    /// Pins comrak sourcepos column semantics: columns must be 1-based BYTE
    /// columns for our LineStarts conversion to be valid on multibyte text.
    /// If this fails, the conversion in `import::LineStarts::range` is wrong.
    #[test]
    fn sourcepos_columns_are_bytes() {
        // 'é' is 2 bytes in UTF-8; emoji is 4.
        let source = "é **b** x\n\n🦀 *i*\n";
        let tree = import(source);
        assert_eq!(tree.blocks.len(), 2);
        // Block ranges must slice the source exactly at paragraph boundaries.
        assert_eq!(
            &source[tree.blocks[0].source_range.clone()],
            "é **b** x",
            "first paragraph range (byte columns expected)"
        );
        assert_eq!(&source[tree.blocks[1].source_range.clone()], "🦀 *i*");
        // The bold run's raw slice must be exactly the source text.
        let bold_run = tree.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run {
                text, raw, marks, ..
            } if marks.contains(MarkSet::BOLD) => Some((text.clone(), raw.clone())),
            _ => None,
        });
        let (text, raw) = bold_run.expect("bold run found");
        assert_eq!(text, "b");
        assert_eq!(raw.as_deref(), Some("b"));
    }

    #[test]
    fn frontmatter_is_root_state_not_a_block() {
        let source = "---\ntitle: Test\n---\n\n# Heading\n";
        let tree = import(source);
        let fm = tree.frontmatter.expect("frontmatter present");
        assert!(fm.raw.contains("title: Test"));
        assert_eq!(tree.blocks.len(), 1);
        assert!(matches!(
            tree.blocks[0].kind,
            BlockKind::Heading {
                level: 1,
                style: HeadingStyle::Atx
            }
        ));
    }

    #[test]
    fn captures_delimiter_fidelity() {
        let tree = import("__bold__ and _italic_ and `code`\n");
        let runs: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Run {
                    text,
                    marks,
                    fidelity,
                    ..
                } => Some((text.as_str(), *marks, *fidelity)),
                _ => None,
            })
            .collect();
        let bold = runs
            .iter()
            .find(|(_, m, _)| m.contains(MarkSet::BOLD))
            .unwrap();
        assert_eq!(bold.2.strong_delim, b'_');
        let italic = runs
            .iter()
            .find(|(_, m, _)| m.contains(MarkSet::ITALIC))
            .unwrap();
        assert_eq!(italic.2.emph_delim, b'_');
        let code = runs
            .iter()
            .find(|(_, m, _)| m.contains(MarkSet::CODE))
            .unwrap();
        assert_eq!(code.0, "code");
        assert_eq!(code.2.code_backticks, 1);
    }

    #[test]
    fn lists_and_tasks_carry_structure() {
        let source = "- one\n- [x] done\n- [ ] todo\n\n1) a\n2) b\n";
        let tree = import(source);
        let bullet = &tree.blocks[0];
        assert!(matches!(
            bullet.kind,
            BlockKind::BulletList { marker: b'-', .. }
        ));
        assert_eq!(bullet.children.len(), 3);
        assert!(matches!(
            bullet.children[0].kind,
            BlockKind::ListItem { task: None }
        ));
        assert!(matches!(
            bullet.children[1].kind,
            BlockKind::ListItem { task: Some(true) }
        ));
        assert!(matches!(
            bullet.children[2].kind,
            BlockKind::ListItem { task: Some(false) }
        ));
        let ordered = &tree.blocks[1];
        assert!(matches!(
            ordered.kind,
            BlockKind::OrderedList {
                start: 1,
                delimiter: b')',
                ..
            }
        ));
    }

    #[test]
    fn code_block_keeps_fence_fidelity_and_literal() {
        let source = "~~~~rust\nfn main() {}\n~~~~\n";
        let tree = import(source);
        match &tree.blocks[0].kind {
            BlockKind::CodeBlock {
                info,
                fence: Some(f),
                literal,
            } => {
                assert_eq!(info, "rust");
                assert_eq!(f.fence_char, b'~');
                assert_eq!(f.fence_length, 4);
                assert_eq!(literal, "fn main() {}\n");
            }
            other => panic!("expected fenced code block, got {other:?}"),
        }
    }

    #[test]
    fn tables_become_row_cell_containers() {
        let source = "| a | b |\n|:--|--:|\n| 1 | 2 |\n";
        let tree = import(source);
        match &tree.blocks[0].kind {
            BlockKind::Table { alignments } => {
                assert_eq!(alignments, &[ColumnAlign::Left, ColumnAlign::Right]);
            }
            other => panic!("expected table, got {other:?}"),
        }
        let rows = &tree.blocks[0].children;
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].kind, BlockKind::TableRow { header: true }));
        assert!(matches!(
            rows[1].kind,
            BlockKind::TableRow { header: false }
        ));
        assert_eq!(rows[0].children.len(), 2);
        assert!(matches!(rows[0].children[0].kind, BlockKind::TableCell));
    }

    #[test]
    fn unknown_constructs_become_opaque_with_exact_slices() {
        let source = "<div class=\"x\">\nraw html\n</div>\n\npara\n";
        let tree = import(source);
        assert!(matches!(tree.blocks[0].kind, BlockKind::Opaque { .. }));
        assert_eq!(
            &source[tree.blocks[0].source_range.clone()],
            "<div class=\"x\">\nraw html\n</div>"
        );
        assert!(matches!(tree.blocks[1].kind, BlockKind::Paragraph));
    }

    #[test]
    fn links_and_images_carry_attrs() {
        let source = "[text](https://e.com \"T\") ![alt](img.png) <https://auto.link>\n";
        let tree = import(source);
        let inlines = &tree.blocks[0].inlines;
        let link_run = inlines
            .iter()
            .find_map(|i| match i {
                Inline::Run {
                    link: Some(l),
                    text,
                    ..
                } => Some((text.clone(), l.clone())),
                _ => None,
            })
            .expect("link run");
        assert_eq!(link_run.0, "text");
        assert_eq!(link_run.1.url, "https://e.com");
        assert_eq!(link_run.1.title.as_deref(), Some("T"));
        assert!(!link_run.1.autolink);
        let image = inlines.iter().find_map(|i| match i {
            Inline::Image { alt, url, .. } => Some((alt.clone(), url.clone())),
            _ => None,
        });
        assert_eq!(image, Some(("alt".into(), "img.png".into())));
        let auto = inlines.iter().find_map(|i| match i {
            Inline::Run { link: Some(l), .. } if l.autolink => Some(l.url.clone()),
            _ => None,
        });
        assert_eq!(auto.as_deref(), Some("https://auto.link"));
    }

    #[test]
    fn hard_break_styles_detected() {
        let tree = import("a  \nb\\\nc\n");
        let breaks: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::HardBreak { style } => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(breaks, vec![BreakStyle::TwoSpaces, BreakStyle::Backslash]);
    }

    #[test]
    fn showcase_fixture_imports_completely() {
        let source = include_str!("../../../markrust-app/tests/fixtures/showcase.md");
        let tree = import(source);
        assert!(tree.frontmatter.is_some(), "showcase has frontmatter");
        // Every construct family from the fixture must be represented.
        fn kinds<'a>(blocks: &'a [Block], out: &mut Vec<&'a BlockKind>) {
            for b in blocks {
                out.push(&b.kind);
                kinds(&b.children, out);
            }
        }
        let mut all = Vec::new();
        kinds(&tree.blocks, &mut all);
        assert!(all.iter().any(|k| matches!(k, BlockKind::Heading { .. })));
        assert!(all.iter().any(|k| matches!(k, BlockKind::Table { .. })));
        assert!(all
            .iter()
            .any(|k| matches!(k, BlockKind::ListItem { task: Some(_) })));
        assert!(all.iter().any(|k| matches!(k, BlockKind::CodeBlock { .. })));
        assert!(all.iter().any(|k| matches!(k, BlockKind::BlockQuote)));
        // Block source ranges of top-level blocks are ordered and in bounds.
        let mut prev_end = 0;
        for b in &tree.blocks {
            assert!(b.source_range.start >= prev_end, "blocks ordered");
            assert!(b.source_range.end <= source.len());
            prev_end = b.source_range.start;
        }
    }

    // === P2: serialization properties ===

    fn preserve(source: &str) -> String {
        let tree = import(source);
        serialize_tree(&tree, source, SerializeMode::Preserve, &Default::default())
    }

    fn normalize(source: &str) -> String {
        let tree = import(source);
        serialize_tree(&tree, source, SerializeMode::Normalize, &Default::default())
    }

    const SAMPLES: &[&str] = &[
        "# H\n\npara with **bold _nested_** text\n",
        "- a\n- b\n  - nested\n\n1. x\n2. y\n",
        "> quote\n>\n> second para\n",
        "| a | b |\n|:--|--:|\n| 1 | 2 |\n",
        "```rust\nfn main() {}\n```\n",
        "---\ntitle: T\n---\n\nbody\n",
        "a  \nb\\\nc\n",
        "task:\n\n- [x] done\n- [ ] todo\n",
        "text with `code` and ~~strike~~ and [link](https://e.com)\n",
        "<div>\nhtml\n</div>\n\npara\n",
    ];

    #[test]
    fn preserve_is_identity_on_fresh_import() {
        for source in SAMPLES {
            assert_eq!(
                &preserve(source),
                source,
                "preserve identity for {source:?}"
            );
        }
        let showcase = include_str!("../../../markrust-app/tests/fixtures/showcase.md");
        assert_eq!(
            preserve(showcase),
            showcase,
            "preserve identity for showcase.md"
        );
    }

    #[test]
    fn normalize_reaches_fixed_point_in_one_step() {
        for source in SAMPLES {
            let once = normalize(source);
            let twice = normalize(&once);
            assert_eq!(once, twice, "normalize fixed point for {source:?}");
        }
        let showcase = include_str!("../../../markrust-app/tests/fixtures/showcase.md");
        let once = normalize(showcase);
        let twice = normalize(&once);
        assert_eq!(once, twice, "normalize fixed point for showcase.md");
    }

    #[test]
    fn normalize_preserves_meaning_via_html_oracle() {
        for source in SAMPLES {
            let html_orig = crate::export::markdown_to_html_gfm(source);
            let html_norm = crate::export::markdown_to_html_gfm(&normalize(source));
            assert_eq!(html_orig, html_norm, "html oracle for {source:?}");
        }
    }
}
