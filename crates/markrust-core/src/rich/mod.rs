// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich document model: structural projection of the markdown source for the
//! WYSIWYG editor. The source string stays the single source of truth; see
//! `docs/architecture.md`.

pub mod command;
pub mod emoji;
pub mod engine;
pub mod entities;
pub mod escape;
pub mod import;
pub mod input_rules;
pub mod save;
pub mod serialize;
pub mod tree;

pub use command::{
    apply_rich_command, code_body_source_map, html_block_literal_source_map,
    place_caret_for_click_below, table_select_all_range, BlockType, CaretState, RichCommand,
    RichError, RichOutcome,
};
pub use emoji::{lookup_emoji, lookup_shortcode};
pub use engine::{
    blank_caret_gap_after_last, blank_caret_gap_at, blank_caret_gap_before, blank_caret_gaps,
    caret_for_click_below_content, line_prefix_parts, list_marker_on_line,
    tagfilter_widget_ranges_in, Bias, BlockSpan, BlockSplice, LinePrefixParts, RichEngine,
    TablePos,
};
pub use entities::{is_decoded_backslash_escape, is_decoded_character_reference};
pub use import::import_markdown;
pub use input_rules::{
    input_rule_breaks_table, match_input_rule, match_input_rule_with, InputRule,
};
pub use save::{save_candidates, DiffHunk, SaveCandidates};
pub use serialize::{serialize_block, serialize_tree, SerializeMode};
pub use tree::{
    alert_title_range, code_span_visible_range, emoji_visible_range, expand_around_html_phrasing,
    expand_around_markdown_link, expand_link_and_html_chrome, expand_marks_and_link_chrome,
    find_alert_chrome, grow_mark_delimiters, is_toc_marker, link_reference_def_chrome,
    markdown_link_chrome, markdown_link_dest_parts, math_visible_range, toc_visible_range,
    wiki_visible_range, AlertChrome, AlertKind, Block, BlockKind, BreakStyle, ColumnAlign,
    FenceFidelity, Frontmatter, HeadingStyle, IdGen, Inline, LinkAttrs, LinkReferenceDefChrome,
    MarkFidelity, MarkSet, MarkdownLinkChrome, NodeId, PrefixBlank, RichTree,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;

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
        for source in [
            "---\ntitle: Test\n---\n\n# Heading\n",
            "---\ntitle: Test\n...\n\n# Heading\n",
        ] {
            let tree = import(source);
            let fm = tree.frontmatter.as_ref().expect("frontmatter present");
            assert!(fm.raw.contains("title: Test"), "{source:?}");
            assert!(
                fm.raw.contains("---") || fm.raw.contains("..."),
                "fences must stay in raw, {source:?} raw={:?}",
                fm.raw
            );
            assert_eq!(tree.blocks.len(), 1, "{source:?}");
            assert!(
                matches!(
                    tree.blocks[0].kind,
                    BlockKind::Heading {
                        level: 1,
                        style: HeadingStyle::Atx
                    }
                ),
                "{source:?}"
            );
            let title = source.find("Test").unwrap();
            assert!(
                title < super::engine::frontmatter_body_start(&tree),
                "YAML title must sit inside the panel range, {source:?}"
            );
        }
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
    fn list_item_x_at_eol_without_space_is_not_a_task() {
        let tree = import("- [x]\n");
        let item = &tree.blocks[0].children[0];
        assert!(
            matches!(item.kind, BlockKind::ListItem { task: None }),
            "GFM requires a space after `]`, got {:?}",
            item.kind
        );
        let para = item
            .children
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph))
            .expect("paragraph");
        let text: String = para
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Run { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("[x]"),
            "[x] at EOL must stay list-item text, got {text:?}"
        );

        let real = import("- [x] done\n");
        assert!(
            matches!(
                real.blocks[0].children[0].kind,
                BlockKind::ListItem { task: Some(true) }
            ),
            "real task with a space after `]` must stay a task"
        );
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
        let body = source.find("fn main() {}").expect("body");
        assert_eq!(
            tree.blocks[0].code_body_range(source),
            body..body + "fn main() {}".len()
        );
    }

    #[test]
    fn indented_code_source_range_includes_opening_indent() {
        for source in ["    indented\n", "\tindented\n"] {
            let tree = import(source);
            let block = &tree.blocks[0];
            match &block.kind {
                BlockKind::CodeBlock { fence: None, .. } => {}
                other => panic!("expected indented code, {source:?} got {other:?}"),
            }
            assert!(
                block.source_range.start < source.find("indented").expect("body"),
                "block must cover the opening indent, {source:?} range={:?}",
                block.source_range
            );
            let body = block.code_body_range(source);
            assert_eq!(
                &source[body.start..body.start + "indented".len()],
                "indented",
                "body must start at content, {source:?} body={body:?}"
            );
            assert_eq!(
                source.as_bytes().get(block.source_range.start),
                source.as_bytes().first(),
                "recovered start must be the indent byte, {source:?}"
            );
        }
    }

    #[test]
    fn cm_opening_indent_is_in_source_range() {
        for source in [" # Title\n", " ```\nfoo\n```\n", " ---\n", " Title\n ===\n"] {
            let tree = import(source);
            assert_eq!(
                tree.blocks[0].source_range.start, 0,
                "0–3 space indent must be in the block span, {source:?} range={:?}",
                tree.blocks[0].source_range
            );
            assert_eq!(source.as_bytes().first(), Some(&b' '), "{source:?}");
        }

        let quoted = ">  # Title\n";
        let tree = import(quoted);
        let heading = tree.blocks[0]
            .children
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Heading { .. }))
            .expect("quoted heading");
        assert!(
            heading.source_range.start > 0,
            "must not steal `>` into the heading, range={:?}",
            heading.source_range
        );
        assert_eq!(
            quoted.as_bytes().get(heading.source_range.start),
            Some(&b' '),
            "heading span starts on the extra indent after `>`, range={:?}",
            heading.source_range
        );
        assert_eq!(quoted.as_bytes().first(), Some(&b'>'));

        let four = "    # Title\n";
        let tree = import(four);
        match &tree.blocks[0].kind {
            BlockKind::CodeBlock { fence: None, .. } => {}
            other => panic!("four spaces must stay indented code, got {other:?}"),
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

    fn inline_debug(tree: &RichTree) -> Vec<String> {
        tree.blocks[0]
            .inlines
            .iter()
            .map(|i| match i {
                Inline::Run { text, .. } => format!("run:{text}"),
                Inline::OpaqueInline { raw, .. } => format!("html:{raw}"),
                Inline::Math {
                    literal, display, ..
                } => format!("math:{}:{literal}", if *display { "$$" } else { "$" }),
                Inline::WikiLink { target, label, .. } => format!("wiki:{target}:{label}"),
                Inline::Emoji { name, glyph, .. } => format!("emoji:{name}:{glyph}"),
                Inline::SoftBreak { .. } => "soft".into(),
                Inline::HardBreak { .. } => "hard".into(),
                Inline::Image { alt, url, .. } => format!("img:{alt}:{url}"),
            })
            .collect()
    }

    #[test]
    fn inline_html_tags_are_opaque_around_inner_text() {
        let tree = import("hello <b>bold</b> world\n");
        assert_eq!(
            inline_debug(&tree),
            vec![
                "run:hello ",
                "html:<b>",
                "run:bold",
                "html:</b>",
                "run: world",
            ]
        );
    }

    #[test]
    fn inline_html_br_and_comment_are_opaque() {
        let br = import("a<br>b\n");
        assert_eq!(inline_debug(&br), vec!["run:a", "html:<br>", "run:b"]);
        let comment = import("a<!-- x -->b\n");
        assert_eq!(
            inline_debug(&comment),
            vec!["run:a", "html:<!-- x -->", "run:b"]
        );
    }

    #[test]
    fn inline_svg_is_one_opaque_image() {
        let source = "hello <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#f00\"/></svg> world\n";
        let tree = import(source);
        let svg = tree.blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::OpaqueInline { raw, .. } => Some(raw.as_ref()),
                _ => None,
            })
            .expect("merged svg");
        assert!(
            svg.contains("<svg") && svg.contains("</svg>"),
            "split HtmlInlines must merge into one svg widget, got {svg:?}"
        );
        assert!(
            crate::html_visual::html_inline_image(svg).is_some(),
            "merged svg must classify as an image, got {svg:?}"
        );
        assert_eq!(
            tree.blocks[0]
                .inlines
                .iter()
                .filter(|i| matches!(i, Inline::OpaqueInline { raw, .. } if raw.contains("<svg")))
                .count(),
            1,
            "must not leave split svg tags, {:?}",
            inline_debug(&tree)
        );
    }

    fn walk_blocks<'a>(blocks: &'a [Block], out: &mut Vec<&'a Block>) {
        for b in blocks {
            out.push(b);
            walk_blocks(&b.children, out);
        }
    }

    fn inlines_have_bold(inlines: &[Inline]) -> bool {
        inlines.iter().any(|i| match i {
            Inline::Run { marks, .. } => marks.contains(MarkSet::BOLD),
            _ => false,
        })
    }

    #[test]
    fn footnote_ref_is_opaque_inline_and_def_is_nested_rich() {
        let tree = import("Hello[^1]\n\n[^1]: the note\n");
        assert_eq!(tree.blocks.len(), 2);
        assert_eq!(inline_debug(&tree), vec!["run:Hello", "html:[^1]"]);
        match &tree.blocks[1].kind {
            BlockKind::FootnoteDefinition { label } => assert_eq!(label, "1"),
            other => panic!("expected footnote def, got {other:?}"),
        }
        assert!(
            !tree.blocks[1].children.is_empty(),
            "footnote body is nested"
        );
        assert_eq!(
            preserve("Hello[^1]\n\n[^1]: the note\n"),
            "Hello[^1]\n\n[^1]: the note\n"
        );
    }

    #[test]
    fn link_reference_definition_is_a_wysiwyg_block() {
        let source = "[hello][ref]\n\n[ref]: https://e.com\n";
        let tree = import(source);
        assert_eq!(tree.blocks.len(), 2, "paragraph + definition, got {tree:?}");
        match &tree.blocks[1].kind {
            BlockKind::LinkReferenceDefinition { label, url, title } => {
                assert_eq!(label, "ref");
                assert_eq!(url, "https://e.com");
                assert!(title.is_none());
            }
            other => panic!("expected link reference definition, got {other:?}"),
        }
        assert_eq!(
            &source[tree.blocks[1].source_range.clone()],
            "[ref]: https://e.com"
        );
        assert_eq!(preserve(source), source);
        let dest_run = tree.blocks[1].inlines.iter().find_map(|i| match i {
            Inline::Run {
                text,
                link: Some(link),
                ..
            } => Some((text.as_str(), link.url.as_str())),
            _ => None,
        });
        assert_eq!(dest_run, Some(("https://e.com", "https://e.com")));
        let hello = tree.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run {
                text,
                link: Some(link),
                ..
            } if text == "hello" => Some(link.url.as_str()),
            _ => None,
        });
        assert_eq!(hello, Some("https://e.com"));
    }

    #[test]
    fn unused_link_reference_definition_is_still_a_block() {
        let source = "[ref]: https://e.com\n";
        let tree = import(source);
        assert_eq!(tree.blocks.len(), 1);
        assert!(matches!(
            tree.blocks[0].kind,
            BlockKind::LinkReferenceDefinition { .. }
        ));
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn empty_link_reference_definition_is_still_a_block() {
        let source = "[hello][ref]\n\n[ref]: \n";
        let tree = import(source);
        assert!(
            tree.blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })),
            "empty `[ref]: ` must stay a definition block, got {:?}",
            tree.blocks.iter().map(|b| &b.kind).collect::<Vec<_>>()
        );
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn link_reference_dest_backslash_wrap_stays_one_definition() {
        let source = "[hello][ref]\n\n[ref]: https://ex.com/pa\\\nth\n";
        let tree = import(source);
        let def = tree.blocks.iter().find_map(|b| match &b.kind {
            BlockKind::LinkReferenceDefinition { url, .. } => Some(url.as_str()),
            _ => None,
        });
        assert_eq!(
            def,
            Some("https://ex.com/path"),
            "dest wrap must concatenate, got {def:?} blocks={:?}",
            tree.blocks.iter().map(|b| &b.kind).collect::<Vec<_>>()
        );
        let hello = tree.blocks.iter().find_map(|b| {
            b.inlines.iter().find_map(|i| match i {
                Inline::Run {
                    text,
                    link: Some(link),
                    ..
                } if text == "hello" => Some(link.url.as_str()),
                _ => None,
            })
        });
        assert_eq!(hello, Some("https://ex.com/path"));
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn link_reference_dest_does_not_swallow_following_setext() {
        let source = "[foo]: /url\nbar\n===\n[foo]\n";
        let tree = import(source);
        let def = tree.blocks.iter().find_map(|b| match &b.kind {
            BlockKind::LinkReferenceDefinition { url, .. } => Some(url.as_str()),
            _ => None,
        });
        assert_eq!(
            def,
            Some("/url"),
            "must not wrap dest into `bar`, got {def:?}"
        );
        assert!(
            tree.blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "setext `bar` / `===` must survive, got {:?}",
            tree.blocks.iter().map(|b| &b.kind).collect::<Vec<_>>()
        );
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn image_reference_definition_is_a_block() {
        let source = "![cat][ref]\n\n[ref]: a.png\n";
        let tree = import(source);
        assert!(
            tree.blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })),
            "image ref dest must be a block, got {:?}",
            tree.blocks.iter().map(|b| &b.kind).collect::<Vec<_>>()
        );
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn quoted_and_list_reference_definitions_round_trip() {
        for source in [
            "> [hello][ref]\n>\n> [ref]: https://e.com\n",
            "- [hello][ref]\n  [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            assert!(
                tree.blocks.iter().any(has_link_ref_def),
                "quoted/list definition must be a block in {source:?}"
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }
    }

    fn linked_text_url(blocks: &[Block], needle: &str) -> Option<String> {
        for block in blocks {
            for inline in &block.inlines {
                if let Inline::Run {
                    text,
                    link: Some(link),
                    ..
                } = inline
                {
                    if text == needle {
                        return Some(link.url.clone());
                    }
                }
            }
            if let Some(url) = linked_text_url(&block.children, needle) {
                return Some(url);
            }
        }
        None
    }

    fn linked_image_url(blocks: &[Block], alt: &str) -> Option<String> {
        for block in blocks {
            for inline in &block.inlines {
                if let Inline::Image { alt: a, url, .. } = inline {
                    if a == alt {
                        return Some(url.clone());
                    }
                }
            }
            if let Some(url) = linked_image_url(&block.children, alt) {
                return Some(url);
            }
        }
        None
    }

    fn run_has_link(blocks: &[Block], needle: &str) -> bool {
        linked_text_url(blocks, needle).is_some()
    }

    #[test]
    fn nested_list_reference_links_keep_link_attrs() {
        for source in [
            "- [hello][ref]\n  [ref]: https://e.com\n",
            "> - [hello][ref]\n>   [ref]: https://e.com\n",
            "- [x] [hello][ref]\n  [ref]: https://e.com\n",
            "- [x] done\n- [hello][ref]\n  [ref]: https://e.com\n",
            "> [hello][ref]\n> [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            assert!(
                tree.blocks.iter().any(has_link_ref_def),
                "definition must stay a block in {source:?}"
            );
            assert_eq!(
                linked_text_url(&tree.blocks, "hello").as_deref(),
                Some("https://e.com"),
                "[hello][ref] must keep Link attrs after nested def recovery, {source:?}"
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }

        let image = "- ![cat][ref]\n  [ref]: a.png\n";
        let tree = import(image);
        assert!(tree.blocks.iter().any(has_link_ref_def));
        assert_eq!(
            linked_image_url(&tree.blocks, "cat").as_deref(),
            Some("a.png"),
            "list-item ![cat][ref] must stay an image after nested def recovery"
        );
        assert_eq!(preserve(image), image);

        let collapsed = "- [foo][]\n  [foo]: https://e.com\n";
        let tree = import(collapsed);
        assert_eq!(
            linked_text_url(&tree.blocks, "foo").as_deref(),
            Some("https://e.com"),
            "collapsed list-item [foo][] must keep Link attrs"
        );

        let shortcut = "- [foo]\n  [foo]: https://e.com\n";
        let tree = import(shortcut);
        assert_eq!(
            linked_text_url(&tree.blocks, "foo").as_deref(),
            Some("https://e.com"),
            "shortcut list-item [foo] must keep Link attrs"
        );

        let task = "- [x] done\n  [x]: https://e.com\n";
        let tree = import(task);
        assert!(
            !run_has_link(&tree.blocks, "x") && !run_has_link(&tree.blocks, "[x]"),
            "- [x] done must not become a shortcut-ref, got {:?}",
            tree.blocks
        );
        assert!(
            tree.blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::BulletList { .. })
                    && b.children
                        .iter()
                        .any(|item| matches!(item.kind, BlockKind::ListItem { task: Some(true) }))),
            "- [x] done must stay a task, got {:?}",
            tree.blocks
        );
        assert_eq!(preserve(task), task);

        let inline_x = "- [x](https://e.com)\n";
        let tree = import(inline_x);
        assert_eq!(
            linked_text_url(&tree.blocks, "x").as_deref(),
            Some("https://e.com")
        );
        assert!(
            !tree.blocks.iter().any(|b| b
                .children
                .iter()
                .any(|item| { matches!(item.kind, BlockKind::ListItem { task: Some(_) }) })),
            "- [x](url) must not be a task"
        );
    }

    fn linked_run_marks(blocks: &[Block], needle: &str) -> Option<(MarkSet, String)> {
        for block in blocks {
            for inline in &block.inlines {
                if let Inline::Run {
                    text,
                    marks,
                    link: Some(link),
                    ..
                } = inline
                {
                    if text == needle {
                        return Some((*marks, link.url.clone()));
                    }
                }
            }
            if let Some(found) = linked_run_marks(&block.children, needle) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn nested_reference_links_keep_inner_marks_and_nested_list_indent() {
        for source in [
            "- [**hello**][ref]\n  [ref]: https://e.com\n",
            "- [x] [**hello**][ref]\n  [ref]: https://e.com\n",
            "> - [**hello**][ref]\n>   [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            let (marks, url) = linked_run_marks(&tree.blocks, "hello").unwrap_or_else(|| {
                panic!(
                    "nested [**hello**][ref] must keep bold Link attrs, {source:?} {:?}",
                    tree.blocks
                )
            });
            assert!(
                marks.contains(MarkSet::BOLD),
                "label emphasis must not flatten, {source:?} marks={marks:?}"
            );
            assert_eq!(url, "https://e.com");
            assert!(
                tree.blocks.iter().any(has_link_ref_def),
                "definition must stay a block in {source:?}"
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }

        let code = "- [`hello`][ref]\n  [ref]: https://e.com\n";
        let tree = import(code);
        let (marks, url) = linked_run_marks(&tree.blocks, "hello").expect("code-label ref");
        assert!(
            marks.contains(MarkSet::CODE),
            "code label must stay code, got {marks:?}"
        );
        assert_eq!(url, "https://e.com");
        assert_eq!(preserve(code), code);

        let image = "- ![*cat*][ref]\n  [ref]: a.png\n";
        let tree = import(image);
        assert_eq!(
            linked_image_url(&tree.blocks, "cat").as_deref(),
            Some("a.png"),
            "nested ![ *cat* ][ref] must stay an image, got {:?}",
            tree.blocks
        );
        assert!(tree.blocks.iter().any(has_link_ref_def));
        assert_eq!(preserve(image), image);

        for source in [
            "- outer\n  - [hello][ref]\n    [ref]: https://e.com\n",
            "1. outer\n   1. [hello][ref]\n      [ref]: https://e.com\n",
            "> - outer\n>   - [hello][ref]\n>     [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            assert_eq!(
                linked_text_url(&tree.blocks, "hello").as_deref(),
                Some("https://e.com"),
                "nested-list [hello][ref] must keep Link attrs, {source:?}"
            );
            assert!(
                tree.blocks.iter().any(has_link_ref_def),
                "nested-list definition must peel, {source:?}"
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }

        let unused = "- outer\n  - done\n    [ref]: https://e.com\n\n[ref]\n";
        let tree = import(unused);
        assert!(
            linked_text_url(&tree.blocks, "ref").is_none()
                && linked_text_url(&tree.blocks, "[ref]").is_none(),
            "unused nested [ref]: must not resolve a later shortcut (182), got {:?}",
            tree.blocks
        );
        assert_eq!(preserve(unused), unused);

        let code_span = "- `[hello][ref]`\n  [ref]: https://e.com\n";
        let tree = import(code_span);
        assert!(
            linked_text_url(&tree.blocks, "hello").is_none(),
            "inline code `[hello][ref]` must not become a link, got {:?}",
            tree.blocks
        );
        assert_eq!(preserve(code_span), code_span);
    }

    fn wrapping_image_link(blocks: &[Block], alt: &str) -> Option<(String, String)> {
        for block in blocks {
            for inline in &block.inlines {
                if let Inline::Image {
                    alt: a,
                    url,
                    link: Some(link),
                    ..
                } = inline
                {
                    if a == alt {
                        return Some((url.clone(), link.url.clone()));
                    }
                }
            }
            if let Some(found) = wrapping_image_link(&block.children, alt) {
                return Some(found);
            }
        }
        None
    }

    fn image_run_marks(blocks: &[Block], alt: &str) -> Option<MarkSet> {
        for block in blocks {
            for inline in &block.inlines {
                if let Inline::Image { alt: a, marks, .. } = inline {
                    if a == alt {
                        return Some(*marks);
                    }
                }
            }
            if let Some(found) = image_run_marks(&block.children, alt) {
                return Some(found);
            }
        }
        None
    }

    fn dest_shortcut_eaten(blocks: &[Block]) -> bool {
        linked_text_url(blocks, "ref").is_some() || linked_text_url(blocks, "[ref]").is_some()
    }

    #[test]
    fn nested_reference_siblings_keep_link_attrs() {
        for source in [
            "> - outer\n>   - [hello][ref]\n>     [ref]: https://e.com\n",
            "- [ ] [**hello**][ref]\n  [ref]: https://e.com\n",
            "1. [**hello**][ref]\n   [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            let (marks, url) = linked_run_marks(&tree.blocks, "hello").unwrap_or_else(|| {
                panic!(
                    "quoted/task/ordered nested ref must keep Link attrs, {source:?} {:?}",
                    tree.blocks
                )
            });
            if source.contains("**hello**") {
                assert!(
                    marks.contains(MarkSet::BOLD),
                    "marked label must stay bold, {source:?} marks={marks:?}"
                );
            }
            assert_eq!(url, "https://e.com");
            assert!(
                !dest_shortcut_eaten(&tree.blocks),
                "dest [ref] must not become a shortcut, {source:?} {:?}",
                tree.blocks
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }

        for source in [
            "- outer\n  - [foo][]\n    [foo]: https://e.com\n",
            "- outer\n  - [foo]\n    [foo]: https://e.com\n",
            "> - outer\n>   - [foo][]\n>     [foo]: https://e.com\n",
            "> - outer\n>   - [foo]\n>     [foo]: https://e.com\n",
            "> [foo]\n> [foo]: https://e.com\n",
            "- [foo]\n  [foo]: https://e.com\n",
        ] {
            let tree = import(source);
            assert_eq!(
                linked_text_url(&tree.blocks, "foo").as_deref(),
                Some("https://e.com"),
                "collapsed/shortcut nested ref must keep Link attrs, {source:?} {:?}",
                tree.blocks
            );
            assert!(
                tree.blocks.iter().any(has_link_ref_def),
                "definition must peel, {source:?}"
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }

        let bold_alt = "- ![**cat**][ref]\n  [ref]: a.png\n";
        let tree = import(bold_alt);
        assert_eq!(
            linked_image_url(&tree.blocks, "cat").as_deref(),
            Some("a.png"),
            "![**cat**][ref] must stay an image, got {:?}",
            tree.blocks
        );
        let marks = image_run_marks(&tree.blocks, "cat").expect("image marks");
        assert!(
            marks.contains(MarkSet::BOLD),
            "image alt emphasis must not flatten, marks={marks:?} {:?}",
            tree.blocks
        );
        assert!(
            !dest_shortcut_eaten(&tree.blocks),
            "image dest [ref] must not become a shortcut, {:?}",
            tree.blocks
        );
        assert_eq!(preserve(bold_alt), bold_alt);

        for source in [
            "- [![cat](a.png)][ref]\n  [ref]: https://e.com\n",
            "> - [![cat](a.png)][ref]\n>   [ref]: https://e.com\n",
            "- [x] [![cat](a.png)][ref]\n  [ref]: https://e.com\n",
            "- [ ] [![cat](a.png)][ref]\n  [ref]: https://e.com\n",
            "- outer\n  - [![cat](a.png)][ref]\n    [ref]: https://e.com\n",
            "[![cat](a.png)][ref]\n[ref]: https://e.com\n",
            "- [![cat][pic]][ref]\n  [pic]: a.png\n  [ref]: https://e.com\n",
        ] {
            let tree = import(source);
            let (img_url, wrap_url) =
                wrapping_image_link(&tree.blocks, "cat").unwrap_or_else(|| {
                    panic!(
                        "[![cat]…][ref] must keep wrapping Link attrs, {source:?} {:?}",
                        tree.blocks
                    )
                });
            assert_eq!(img_url, "a.png");
            assert_eq!(wrap_url, "https://e.com");
            assert!(
                !dest_shortcut_eaten(&tree.blocks),
                "wrapping dest [ref] must not become a shortcut, {source:?} {:?}",
                tree.blocks
            );
            assert_eq!(preserve(source), source, "preserve {source:?}");
        }
    }

    #[test]
    fn titled_link_dest_parts_skip_wrapping_chrome() {
        for (source, url, title) in [
            (
                "[label](https://e.com \"title\")\n",
                "https://e.com",
                "title",
            ),
            ("[label](https://e.com 'title')\n", "https://e.com", "title"),
            ("[label](https://e.com (title))\n", "https://e.com", "title"),
            (
                "[label](<https://e.com> \"title\")\n",
                "https://e.com",
                "title",
            ),
            ("![alt](a.png \"title\")\n", "a.png", "title"),
        ] {
            let span = 0..source.trim_end().len();
            let chrome = markdown_link_chrome(source, span).expect("chrome");
            let parts = markdown_link_dest_parts(source, chrome.dest).expect("dest parts");
            assert_eq!(&source[parts.url.clone()], url, "url inner, {source:?}");
            let title_range = parts.title.clone().expect("title inner");
            assert_eq!(
                &source[title_range.clone()],
                title,
                "title inner, {source:?}"
            );
            let dest_open = source.find('(').expect("(");
            assert_eq!(
                parts.snap(dest_open),
                Some(parts.url.start),
                "`(` snaps onto the URL, {source:?}"
            );
            let quote = source[..title_range.start]
                .rfind(['"', '\'', '('])
                .expect("title opener");
            assert_eq!(
                parts.snap(quote),
                Some(title_range.start),
                "title opener snaps onto title inner, {source:?}"
            );
        }
    }

    #[test]
    fn emphasis_wrapping_a_link_keeps_marks_and_expands_outer() {
        for (source, want_mark) in [
            ("**[hello](https://e.com)**\n", MarkSet::BOLD),
            ("*[hello](https://e.com)*\n", MarkSet::ITALIC),
            ("~~[hello](https://e.com)~~\n", MarkSet::STRIKE),
            ("[**hello**](https://e.com)\n", MarkSet::BOLD),
        ] {
            let tree = import(source);
            let (marks, url) = linked_run_marks(&tree.blocks, "hello")
                .unwrap_or_else(|| panic!("expected linked hello, {source:?} {:?}", tree.blocks));
            assert_eq!(url, "https://e.com");
            assert!(
                marks.contains(want_mark),
                "{source:?} marks={marks:?} want {want_mark:?}"
            );
            let hello = source.find("hello").expect("hello");
            let inner = hello..hello + "hello".len();
            let link = LinkAttrs {
                url: url.clone(),
                title: None,
                autolink: false,
                angle: false,
                group: 1,
            };
            let outer = expand_marks_and_link_chrome(
                source,
                inner,
                Some(&link),
                0,
                source.trim_end().len(),
            );
            let slice = &source[outer.clone()];
            assert!(
                slice.starts_with("**[")
                    || slice.starts_with("*[")
                    || slice.starts_with("~~[")
                    || slice.starts_with("[**"),
                "outer must include wrapping marks and `[`, {source:?} got {slice:?}"
            );
            assert!(
                slice.contains("https://e.com"),
                "outer must include dest, {source:?} got {slice:?}"
            );
        }

        let nested = "***[hello](https://e.com)***\n";
        let hello = nested.find("hello").expect("hello");
        let link = LinkAttrs {
            url: "https://e.com".into(),
            title: None,
            autolink: false,
            angle: false,
            group: 1,
        };
        let outer = expand_marks_and_link_chrome(
            nested,
            hello..hello + "hello".len(),
            Some(&link),
            0,
            nested.trim_end().len(),
        );
        assert_eq!(
            &nested[outer], "***[hello](https://e.com)***",
            "nested `***` wrapping a link must expand as dest chrome"
        );

        let html = "**<b>hello</b>**\n";
        let hello = html.find("hello").expect("hello");
        let outer = expand_marks_and_link_chrome(
            html,
            hello..hello + "hello".len(),
            None,
            0,
            html.trim_end().len(),
        );
        assert_eq!(
            &html[outer], "**<b>hello</b>**",
            "wrap marks around HTML phrasing must expand as dest chrome"
        );

        let linked_html = "[<b>hello</b>](https://e.com)\n";
        let hello = linked_html.find("hello").expect("hello");
        let link = LinkAttrs {
            url: "https://e.com".into(),
            title: None,
            autolink: false,
            angle: false,
            group: 1,
        };
        let outer = expand_marks_and_link_chrome(
            linked_html,
            hello..hello + "hello".len(),
            Some(&link),
            0,
            linked_html.trim_end().len(),
        );
        assert_eq!(
            &linked_html[outer], "[<b>hello</b>](https://e.com)",
            "link wrapping HTML phrasing must expand dest around tags"
        );
    }

    fn has_link_ref_def(block: &Block) -> bool {
        matches!(block.kind, BlockKind::LinkReferenceDefinition { .. })
            || block.children.iter().any(has_link_ref_def)
    }

    #[test]
    fn lazy_paragraph_ref_def_does_not_resolve_later_shortcut() {
        // CommonMark example 182: a definition cannot interrupt a paragraph.
        let source = "Foo\n[bar]: /baz\n\n[bar]\n";
        let tree = import(source);
        assert!(
            linked_text_url(&tree.blocks, "bar").is_none(),
            "later `[bar]` must stay text, got {:?}",
            tree.blocks
        );
        assert_eq!(preserve(source), source);
        let escaped = "\\[foo]\n\n[foo]: /url \"title\"\n";
        let tree = import(escaped);
        assert!(
            linked_text_url(&tree.blocks, "foo").is_none(),
            "escaped `\\[foo]` must not become a shortcut-ref"
        );
        assert_eq!(preserve(escaped), escaped);
    }

    #[test]
    fn unmatched_footnote_ref_imports_as_opaque() {
        let source = "Hello[^1] world\n";
        let tree = import(source);
        assert_eq!(
            inline_debug(&tree),
            vec!["run:Hello", "html:[^1]", "run: world"],
            "unmatched [^1] must import as a footnote-ref opaque, got {:?}",
            inline_debug(&tree)
        );
        assert_eq!(preserve(source), source);

        let linked = import("Hello[^1](https://e.com)\n");
        assert!(
            linked.blocks[0].inlines.iter().any(|i| match i {
                Inline::Run {
                    link: Some(l),
                    text,
                    ..
                } => l.url == "https://e.com" && (text == "^1" || text.contains("^1")),
                _ => false,
            }),
            "[^1](url) must stay a link, got {:?}",
            inline_debug(&linked)
        );
        assert!(
            !linked.blocks[0].inlines.iter().any(|i| match i {
                Inline::OpaqueInline { raw, .. } => {
                    crate::html_visual::footnote_ref_label(raw).is_some()
                }
                _ => false,
            }),
            "[^1](url) must not become a footnote ref, got {:?}",
            inline_debug(&linked)
        );

        let code = import("`[^1]`\n");
        assert!(
            !code.blocks[0].inlines.iter().any(|i| match i {
                Inline::OpaqueInline { raw, .. } => {
                    crate::html_visual::footnote_ref_label(raw).is_some()
                }
                _ => false,
            }),
            "inline code must not become a footnote, got {:?}",
            inline_debug(&code)
        );
        assert_eq!(preserve("`[^1]`\n"), "`[^1]`\n");

        let escaped = import("Hello\\[^1]\n");
        assert!(
            !escaped.blocks[0].inlines.iter().any(|i| match i {
                Inline::OpaqueInline { raw, .. } => {
                    crate::html_visual::footnote_ref_label(raw).is_some()
                }
                _ => false,
            }),
            "escaped \\[^1] must stay text, got {:?}",
            inline_debug(&escaped)
        );
        assert_eq!(preserve("Hello\\[^1]\n"), "Hello\\[^1]\n");
    }

    #[test]
    fn footnote_def_body_parses_bold_link_and_code() {
        let source = "See[^1]\n\n[^1]: **bold** and `code` and [a](https://e.com)\n";
        let tree = import(source);
        let def = tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::FootnoteDefinition { .. }))
            .expect("footnote def");
        let mut all = Vec::new();
        walk_blocks(&def.children, &mut all);
        let body = all
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph))
            .expect("footnote body paragraph");
        assert!(
            inlines_have_bold(&body.inlines),
            "expected nested bold, got {:?}",
            body.inlines
        );
        let has_code = body.inlines.iter().any(|i| match i {
            Inline::Run { marks, text, .. } => marks.contains(MarkSet::CODE) && text == "code",
            _ => false,
        });
        assert!(has_code, "expected nested code, got {:?}", body.inlines);
        let has_link = body.inlines.iter().any(|i| match i {
            Inline::Run {
                link: Some(l),
                text,
                ..
            } => text == "a" && l.url == "https://e.com",
            _ => false,
        });
        assert!(has_link, "expected nested link, got {:?}", body.inlines);
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn definition_list_is_nested_rich() {
        let blank = import("Term\n\n: Definition\n");
        match &blank.blocks[0].kind {
            BlockKind::DefinitionList => {}
            other => panic!("expected definition list, got {other:?}"),
        }
        let tight = import("Term\n: Definition\n");
        assert!(matches!(tight.blocks[0].kind, BlockKind::DefinitionList));
        assert_eq!(preserve("Term\n\n: Definition\n"), "Term\n\n: Definition\n");
        assert_eq!(preserve("Term\n: Definition\n"), "Term\n: Definition\n");
    }

    #[test]
    fn definition_details_parse_bold_link_and_code() {
        let source = "Term\n\n: **bold** and `code` and [a](https://e.com)\n";
        let tree = import(source);
        let mut all = Vec::new();
        walk_blocks(&tree.blocks, &mut all);
        let details = all
            .iter()
            .find(|b| matches!(b.kind, BlockKind::DefinitionDetails))
            .expect("definition details");
        let mut nested = Vec::new();
        walk_blocks(&details.children, &mut nested);
        let body = nested
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph))
            .expect("details paragraph");
        assert!(
            inlines_have_bold(&body.inlines),
            "expected nested bold in details, got {:?}",
            body.inlines
        );
        let has_code = body.inlines.iter().any(|i| match i {
            Inline::Run { marks, text, .. } => marks.contains(MarkSet::CODE) && text == "code",
            _ => false,
        });
        assert!(has_code, "expected nested code, got {:?}", body.inlines);
        let has_link = body.inlines.iter().any(|i| match i {
            Inline::Run {
                link: Some(l),
                text,
                ..
            } => text == "a" && l.url == "https://e.com",
            _ => false,
        });
        assert!(has_link, "expected nested link, got {:?}", body.inlines);
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn highlight_eqeq_is_a_mark_not_source_chrome() {
        let tree = import("hello ==mark== world\n");
        let highlighted = tree.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run { text, marks, .. } if marks.contains(MarkSet::HIGHLIGHT) => {
                Some(text.clone())
            }
            _ => None,
        });
        assert_eq!(highlighted.as_deref(), Some("mark"));
        assert!(!tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { text, .. } => text.contains("=="),
            Inline::OpaqueInline { raw, .. } => raw.contains("=="),
            _ => false,
        }));
        assert_eq!(preserve("hello ==mark== world\n"), "hello ==mark== world\n");
        let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
        let rewritten = serialize_tree(
            &tree,
            "hello ==mark== world\n",
            SerializeMode::Preserve,
            &dirty,
        );
        assert!(
            rewritten.contains("==mark=="),
            "dirty serialize must keep == delimiters, got {rewritten:?}"
        );
    }

    #[test]
    fn highlight_eqeq_nests_bold() {
        let tree = import("==**bold**==\n");
        let run = tree.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run { text, marks, .. } => Some((text.clone(), *marks)),
            _ => None,
        });
        let (text, marks) = run.expect("run");
        assert_eq!(text, "bold");
        assert!(marks.contains(MarkSet::BOLD));
        assert!(marks.contains(MarkSet::HIGHLIGHT));
        assert_eq!(preserve("==**bold**==\n"), "==**bold**==\n");
    }

    #[test]
    fn superscript_and_subscript_import() {
        let sub = import("H~2~O\n");
        let has_sub = sub.blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { text, marks, .. } => marks.contains(MarkSet::SUB) && text == "2",
            _ => false,
        });
        assert!(
            has_sub,
            "expected subscript 2, got {:?}",
            sub.blocks[0].inlines
        );
        let sup = import("mc^2^\n");
        let has_sup = sup.blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { text, marks, .. } => marks.contains(MarkSet::SUP) && text == "2",
            _ => false,
        });
        assert!(
            has_sup,
            "expected superscript 2, got {:?}",
            sup.blocks[0].inlines
        );
        assert_eq!(preserve("H~2~O\n"), "H~2~O\n");
        assert_eq!(preserve("mc^2^\n"), "mc^2^\n");
    }

    #[test]
    fn math_dollars_are_first_class_not_opaque() {
        let source = "see $x^2$ and $$E=mc^2$$ and $5\n";
        let tree = import(source);
        let maths: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Math {
                    literal,
                    display,
                    raw,
                    ..
                } => Some((literal.as_str(), *display, raw.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            maths,
            vec![("x^2", false, "$x^2$"), ("E=mc^2", true, "$$E=mc^2$$")]
        );
        assert!(!tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::Math { .. } => false,
            Inline::Run { text, .. } => text.contains("$x^2$") || text.contains("$$"),
            Inline::OpaqueInline { raw, .. } => raw.contains("$x"),
            _ => false,
        }));
        assert!(tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { text, .. } => text.contains("$5"),
            _ => false,
        }));
        assert_eq!(preserve(source), source);
        let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
        let rewritten = serialize_tree(&tree, source, SerializeMode::Preserve, &dirty);
        assert!(
            rewritten.contains("$x^2$")
                && rewritten.contains("$$E=mc^2$$")
                && rewritten.contains("$5"),
            "dirty serialize must keep dollar math, got {rewritten:?}"
        );
    }

    #[test]
    fn wikilinks_are_first_class_not_opaque() {
        let source = "see [[page]] and [[page|Label]]\n";
        let tree = import(source);
        let wikis: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::WikiLink {
                    target, label, raw, ..
                } => Some((target.as_str(), label.as_str(), raw.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            wikis,
            vec![
                ("page", "page", "[[page]]"),
                ("page", "Label", "[[page|Label]]")
            ]
        );
        assert!(!tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::WikiLink { .. } => false,
            Inline::Run { text, .. } => text.contains("[["),
            Inline::OpaqueInline { raw, .. } => raw.contains("[["),
            _ => false,
        }));
        assert_eq!(preserve(source), source);
        let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
        let rewritten = serialize_tree(&tree, source, SerializeMode::Preserve, &dirty);
        assert!(
            rewritten.contains("[[page]]") && rewritten.contains("[[page|Label]]"),
            "dirty serialize must keep wikilinks, got {rewritten:?}"
        );
    }

    #[test]
    fn emoji_shortcodes_are_first_class_not_opaque() {
        let source = "hi :smile: and :heart: and :+1: :rocket:\n";
        let tree = import(source);
        let emojis: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Emoji {
                    name, glyph, raw, ..
                } => Some((name.as_str(), glyph.as_str(), raw.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            emojis,
            vec![
                ("smile", "😄", ":smile:"),
                ("heart", "❤️", ":heart:"),
                ("+1", "👍", ":+1:"),
                ("rocket", "🚀", ":rocket:"),
            ]
        );
        assert!(!tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::Emoji { .. } => false,
            Inline::Run { text, .. } => text.contains(":smile:") || text.contains(":rocket:"),
            Inline::OpaqueInline { raw, .. } => raw.contains(":smile:"),
            _ => false,
        }));
        assert_eq!(preserve(source), source);
        let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
        let rewritten = serialize_tree(&tree, source, SerializeMode::Preserve, &dirty);
        assert!(
            rewritten.contains(":smile:")
                && rewritten.contains(":heart:")
                && rewritten.contains(":+1:")
                && rewritten.contains(":rocket:"),
            "dirty serialize must keep :name:, got {rewritten:?}"
        );
        assert!(
            !rewritten.contains("😄") && !rewritten.contains("🚀"),
            "disk form must stay shortcodes, got {rewritten:?}"
        );
    }

    #[test]
    fn unknown_emoji_shortcode_stays_text() {
        let source = "see :not_an_emoji: here\n";
        let tree = import(source);
        assert!(
            !tree.blocks[0]
                .inlines
                .iter()
                .any(|i| matches!(i, Inline::Emoji { .. })),
            "unknown :foo: must not be Inline::Emoji, got {:?}",
            tree.blocks[0].inlines
        );
        assert!(tree.blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { text, .. } => text.contains(":not_an_emoji:"),
            _ => false,
        }));
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn emoji_shortcode_inside_code_is_not_emoji() {
        for source in ["`:smile:`\n", "```\n:smile:\n```\n"] {
            let tree = import(source);
            let has_emoji = tree
                .blocks
                .iter()
                .any(|b| b.inlines.iter().any(|i| matches!(i, Inline::Emoji { .. })));
            assert!(
                !has_emoji,
                "expected no emoji in {source:?}, got {:?}",
                tree.blocks
            );
            assert_eq!(preserve(source), source);
        }
    }

    #[test]
    fn toc_marker_is_a_block_and_roundtrips() {
        for source in ["[TOC]\n", "[toc]\n", "[[toc]]\n", "[[TOC]]\n"] {
            let tree = import(source);
            match &tree.blocks[0].kind {
                BlockKind::Toc { wiki } => {
                    assert_eq!(
                        *wiki,
                        source.trim().eq_ignore_ascii_case("[[toc]]"),
                        "{source}"
                    );
                }
                other => panic!("expected Toc for {source:?}, got {other:?}"),
            }
            assert_eq!(preserve(source), source, "{source:?}");
            let inner = toc_visible_range(source, tree.blocks[0].source_range.clone());
            let name = &source[inner.clone()];
            assert!(
                name.eq_ignore_ascii_case("toc"),
                "TOC inner must be the name, {source:?} got {name:?}"
            );
            assert!(
                !name.contains('[') && !name.contains(']'),
                "TOC brackets are dest chrome, {source:?} inner {name:?}"
            );
        }
        let mixed = "# One\n\n[TOC]\n\n## Two\n";
        let tree = import(mixed);
        assert!(matches!(
            tree.blocks[1].kind,
            BlockKind::Toc { wiki: false }
        ));
        let outline = tree.outline();
        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].2, "One");
        assert_eq!(outline[1].2, "Two");
        assert_eq!(preserve(mixed), mixed);
        let heading_wiki = "# [[page|Hello]]\n";
        let tree = import(heading_wiki);
        assert_eq!(tree.outline()[0].2, "Hello");
    }

    #[test]
    fn github_alerts_import_each_kind_and_roundtrip() {
        for kind in AlertKind::ALL {
            let source = format!("> [!{}]\n> body {}\n", kind.tag(), kind.tag());
            let tree = import(&source);
            match &tree.blocks[0].kind {
                BlockKind::Alert {
                    kind: got,
                    title,
                    tag_range,
                    chrome_range,
                } => {
                    assert_eq!(*got, kind, "{}", kind.tag());
                    assert!(title.is_none(), "{}", kind.tag());
                    assert_eq!(
                        &source[tag_range.clone()],
                        format!("[!{}]", kind.tag()),
                        "{}",
                        kind.tag()
                    );
                    assert_eq!(
                        &source[chrome_range.clone()],
                        format!("[!{}]", kind.tag()),
                        "{}",
                        kind.tag()
                    );
                }
                other => panic!("expected Alert {}, got {other:?}", kind.tag()),
            }
            let body = tree.blocks[0]
                .children
                .iter()
                .find(|b| matches!(b.kind, BlockKind::Paragraph))
                .expect("alert body paragraph");
            let has_tag = body.inlines.iter().any(|i| match i {
                Inline::Run { text, .. } => text.contains("[!"),
                Inline::OpaqueInline { raw, .. } => raw.contains("[!"),
                _ => false,
            });
            assert!(
                !has_tag,
                "[!{}] must not be body text, inlines={:?}",
                kind.tag(),
                body.inlines
            );
            assert_eq!(preserve(&source), source, "{}", kind.tag());
            let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
            let rewritten = serialize_tree(&tree, &source, SerializeMode::Preserve, &dirty);
            assert!(
                rewritten.contains(&format!("[!{}]", kind.tag())) && rewritten.contains("body"),
                "dirty serialize must keep [!{}], got {rewritten:?}",
                kind.tag()
            );
            let html = crate::export::markdown_to_html_gfm(&source);
            let class = format!("markdown-alert-{}", kind.tag().to_ascii_lowercase());
            assert!(
                html.contains(&class) && html.contains("markdown-alert-title"),
                "html for {}: {html}",
                kind.tag()
            );
            assert!(
                !html.contains(&format!("[!{}]", kind.tag())),
                "exported html must not show raw [!{}]: {html}",
                kind.tag()
            );
        }
    }

    #[test]
    fn github_alert_custom_title_and_lowercase_tag() {
        let titled = "> [!NOTE] Pay attention\n> body\n";
        let tree = import(titled);
        match &tree.blocks[0].kind {
            BlockKind::Alert {
                kind,
                title,
                chrome_range,
                ..
            } => {
                assert_eq!(*kind, AlertKind::Note);
                assert_eq!(title.as_deref(), Some("Pay attention"));
                assert_eq!(&titled[chrome_range.clone()], "[!NOTE] Pay attention");
                assert_eq!(kind.callout_label(title.as_deref()), "Pay attention");
            }
            other => panic!("expected titled Note alert, got {other:?}"),
        }
        assert_eq!(preserve(titled), titled);

        let lower = "> [!warning]\n> watch out\n";
        let tree = import(lower);
        match &tree.blocks[0].kind {
            BlockKind::Alert { kind, .. } => assert_eq!(*kind, AlertKind::Warning),
            other => panic!("expected Warning, got {other:?}"),
        }
        assert_eq!(preserve(lower), lower);
        let dirty = std::collections::HashSet::from([tree.blocks[0].id]);
        let rewritten = serialize_tree(&tree, lower, SerializeMode::Preserve, &dirty);
        assert!(
            rewritten.contains("[!WARNING]"),
            "dirty serialize uppercases the tag, got {rewritten:?}"
        );
    }

    #[test]
    fn ordinary_blockquote_is_not_an_alert() {
        let source = "> just a quote\n";
        let tree = import(source);
        assert!(
            matches!(tree.blocks[0].kind, BlockKind::BlockQuote),
            "got {:?}",
            tree.blocks[0].kind
        );
        assert_eq!(preserve(source), source);
    }

    #[test]
    fn currency_and_code_are_not_math() {
        for source in [
            "costs $5\n",
            "$20,000 and $30,000\n",
            "$ a^2 $\n",
            "`$1+2$`\n",
            "```\n$x$\n```\n",
        ] {
            let tree = import(source);
            let has_math = tree
                .blocks
                .iter()
                .any(|b| b.inlines.iter().any(|i| matches!(i, Inline::Math { .. })));
            assert!(
                !has_math,
                "expected no math in {source:?}, got {:?}",
                tree.blocks
            );
            assert_eq!(preserve(source), source);
        }
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
        assert!(!link_run.1.angle);
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
        let auto_attrs = inlines.iter().find_map(|i| match i {
            Inline::Run { link: Some(l), .. } if l.autolink => Some(l.clone()),
            _ => None,
        });
        assert!(
            auto_attrs.is_some_and(|l| l.angle),
            "angle-bracket autolink must set LinkAttrs.angle"
        );

        let email = import("<user@example.com>\n");
        let email_link = email.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run { link: Some(l), .. } => Some(l.clone()),
            _ => None,
        });
        assert!(
            email_link.as_ref().is_some_and(|l| l.angle),
            "email autolink `<>` must set angle, got {email_link:?}"
        );

        let bare = import("https://example.com\n");
        let bare_link = bare.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run { link: Some(l), .. } => Some(l.clone()),
            _ => None,
        });
        assert!(
            bare_link.as_ref().is_some_and(|l| l.autolink && !l.angle),
            "bare GFM autolink must not set angle, got {bare_link:?}"
        );
    }

    fn first_link_run(tree: &RichTree) -> Option<(&str, &LinkAttrs, Range<usize>)> {
        fn walk(blocks: &[Block]) -> Option<(&str, &LinkAttrs, Range<usize>)> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::Run {
                        text,
                        link: Some(l),
                        source_range,
                        ..
                    } = inline
                    {
                        return Some((text.as_str(), l, source_range.clone()));
                    }
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&tree.blocks)
    }

    /// Comrak reports GFM `www.` / bare URL / email sourcepos as `0..1`.
    /// Import recovers the literal so caret/click map onto the URL.
    #[test]
    fn gfm_extended_autolink_source_range_is_the_url_literal() {
        let cases = [
            (
                "see www.example.com now\n",
                "www.example.com",
                "http://www.example.com",
            ),
            (
                "see https://example.com now\n",
                "https://example.com",
                "https://example.com",
            ),
            (
                "see user@example.com now\n",
                "user@example.com",
                "mailto:user@example.com",
            ),
            (
                "> www.example.com\n",
                "www.example.com",
                "http://www.example.com",
            ),
            (
                "- www.example.com\n",
                "www.example.com",
                "http://www.example.com",
            ),
            (
                "www.example.com\n",
                "www.example.com",
                "http://www.example.com",
            ),
            (
                "see www.example.com. now\n",
                "www.example.com",
                "http://www.example.com",
            ),
        ];
        for (source, needle, url) in cases {
            let tree = import(source);
            let (text, link, range) = first_link_run(&tree).expect(source);
            assert_eq!(text, needle, "{source:?}");
            assert_eq!(link.url, url, "{source:?}");
            assert!(
                !link.angle,
                "GFM extended autolink must not invent `<>`, {source:?} got {link:?}"
            );
            if needle.contains("://") {
                assert!(
                    link.autolink,
                    "scheme autolink dest equals visible text, {source:?} got {link:?}"
                );
            } else {
                assert!(
                    !link.autolink,
                    "www/email dest is http:// or mailto: (not the visible text), {source:?} got {link:?}"
                );
            }
            assert_eq!(
                source.get(range.clone()).unwrap_or(""),
                needle,
                "source_range must be the URL literal, {source:?} got {range:?}"
            );
            let start = source.find(needle).expect(needle);
            assert_eq!(range, start..start + needle.len(), "{source:?}");
            // Neighbor runs must not overlap the URL (that panics next_caret).
            fn overlaps(blocks: &[Block], url: &Range<usize>) -> Vec<Range<usize>> {
                let mut out = Vec::new();
                for b in blocks {
                    for inline in &b.inlines {
                        let r = inline.source_range();
                        if r != *url && r.start < url.end && r.end > url.start {
                            out.push(r);
                        }
                    }
                    out.extend(overlaps(&b.children, url));
                }
                out
            }
            let hit = overlaps(&tree.blocks, &range);
            assert!(
                hit.is_empty(),
                "neighbor runs must not overlap the autolink, {source:?} {hit:?}"
            );
        }

        let wrapped_src = "see [www.example.com](https://e.com) now\n";
        let wrapped = import(wrapped_src);
        let (text, link, range) = first_link_run(&wrapped).expect("markdown www label");
        assert!(
            !link.autolink,
            "markdown `[www](url)` must not become a GFM autolink, got {link:?} text={text:?}"
        );
        assert_eq!(link.url, "https://e.com");
        assert!(
            wrapped_src
                .get(range.clone())
                .is_some_and(|s| !s.is_empty()),
            "label run must have a real source slice, got {range:?}"
        );
    }

    #[test]
    fn two_gfm_www_autolinks_keep_distinct_ranges() {
        let source = "www.a.com and www.b.com\n";
        let tree = import(source);
        let mut urls = Vec::new();
        fn collect(blocks: &[Block], out: &mut Vec<(String, Range<usize>)>) {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::Run {
                        text,
                        link: Some(_),
                        source_range,
                        ..
                    } = inline
                    {
                        out.push((text.clone(), source_range.clone()));
                    }
                }
                collect(&b.children, out);
            }
        }
        collect(&tree.blocks, &mut urls);
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert_eq!(urls[0].0, "www.a.com");
        assert_eq!(urls[1].0, "www.b.com");
        assert_eq!(&source[urls[0].1.clone()], "www.a.com");
        assert_eq!(&source[urls[1].1.clone()], "www.b.com");
        assert!(
            urls[0].1.end <= urls[1].1.start,
            "autolinks must not overlap, {urls:?}"
        );
    }

    #[test]
    fn character_reference_runs_keep_entity_source_range() {
        let cases = [
            ("A&amp;B\n", "&amp;", "&"),
            ("A&amp;\n", "&amp;", "&"),
            ("hello &amp;\n", "&amp;", "&"),
            ("A&lt;B\n", "&lt;", "<"),
            ("A&gt;B\n", "&gt;", ">"),
            ("A&quot;B\n", "&quot;", "\""),
            ("A&#39;B\n", "&#39;", "'"),
            ("A&#123;B\n", "&#123;", "{"),
            ("A&#x7B;B\n", "&#x7B;", "{"),
            ("A&#38;\n", "&#38;", "&"),
            ("> A&amp;B\n", "&amp;", "&"),
            ("> A&amp;\n", "&amp;", "&"),
            ("- A&amp;B\n", "&amp;", "&"),
            ("- A&amp;\n", "&amp;", "&"),
            ("[A&amp;B](https://e.com)\n", "&amp;", "&"),
            ("[A&amp;](https://e.com)\n", "&amp;", "&"),
            ("| A&amp;B | x |\n| --- | --- |\n", "&amp;", "&"),
            ("| A&amp; | x |\n| --- | --- |\n", "&amp;", "&"),
        ];
        for (source, literal, decoded) in cases {
            let tree = import(source);
            let found = first_entity_run(&tree, source, literal);
            let (text, range) = found.unwrap_or_else(|| {
                let mut runs = Vec::new();
                dump_runs(&tree.blocks, source, &mut runs);
                panic!("entity run in {source:?}, runs={runs:?}")
            });
            assert_eq!(text, decoded, "{source:?}");
            assert_eq!(
                source.get(range.clone()).unwrap_or(""),
                literal,
                "source_range must be the entity literal, {source:?} got {range:?}"
            );
        }

        let code = import("`A&amp;B`\n");
        let code_run = code.blocks[0].inlines.iter().find_map(|i| match i {
            Inline::Run { text, marks, .. } if marks.contains(MarkSet::CODE) => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            code_run.as_deref(),
            Some("A&amp;B"),
            "code spans must keep the entity literal, got {code_run:?}"
        );
    }

    #[test]
    fn backslash_escape_runs_keep_source_range() {
        let cases = [
            "A\\*B\n",
            "A\\*\n",
            "> A\\*B\n",
            "- A\\*B\n",
            "[A\\*B](https://e.com)\n",
            "| A\\*B | x |\n| --- | --- |\n",
        ];
        for source in cases {
            let tree = import(source);
            let found = first_entity_run(&tree, source, "\\*");
            let (text, range) = found.unwrap_or_else(|| {
                let mut runs = Vec::new();
                dump_runs(&tree.blocks, source, &mut runs);
                panic!("escape run in {source:?}, runs={runs:?}")
            });
            assert_eq!(text, "*", "{source:?}");
            assert_eq!(
                source.get(range.clone()).unwrap_or(""),
                "\\*",
                "source_range must be the escape literal, {source:?} got {range:?}"
            );
        }

        let escaped = "A\\\\\n";
        let tree = import(escaped);
        let found = first_entity_run(&tree, escaped, "\\\\");
        let (text, range) = found.unwrap_or_else(|| {
            let mut runs = Vec::new();
            dump_runs(&tree.blocks, escaped, &mut runs);
            panic!("escape run in {escaped:?}, runs={runs:?}")
        });
        assert_eq!(text, "\\", "{escaped:?}");
        assert_eq!(
            escaped.get(range.clone()).unwrap_or(""),
            "\\\\",
            "last-in-line `\\\\` source_range must be the escape literal, got {range:?}"
        );
    }

    fn first_entity_run(
        tree: &RichTree,
        source: &str,
        literal: &str,
    ) -> Option<(String, Range<usize>)> {
        fn walk(blocks: &[Block], source: &str, literal: &str) -> Option<(String, Range<usize>)> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::Run {
                        text,
                        source_range,
                        marks,
                        ..
                    } = inline
                    {
                        if marks.contains(MarkSet::CODE) {
                            continue;
                        }
                        if source.get(source_range.clone()) == Some(literal) {
                            return Some((text.clone(), source_range.clone()));
                        }
                    }
                }
                if let Some(found) = walk(&b.children, source, literal) {
                    return Some(found);
                }
            }
            None
        }
        walk(&tree.blocks, source, literal)
    }

    fn dump_runs(blocks: &[Block], source: &str, out: &mut Vec<(String, String, Range<usize>)>) {
        for b in blocks {
            for inline in &b.inlines {
                if let Inline::Run {
                    text, source_range, ..
                } = inline
                {
                    out.push((
                        text.clone(),
                        source.get(source_range.clone()).unwrap_or("").to_string(),
                        source_range.clone(),
                    ));
                }
            }
            dump_runs(&b.children, source, out);
        }
    }

    #[test]
    fn hard_break_styles_detected() {
        let tree = import("a  \nb\\\nc\n");
        let breaks: Vec<_> = tree.blocks[0]
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::HardBreak { style, .. } => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(breaks, vec![BreakStyle::TwoSpaces, BreakStyle::Backslash]);
    }

    #[test]
    fn soft_and_hard_breaks_carry_source_range_on_the_break() {
        let source = "hello\nworld\n";
        let tree = import(source);
        let soft = tree.blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::SoftBreak { source_range } => Some(source_range.clone()),
                _ => None,
            })
            .expect("soft break");
        assert_eq!(
            source.get(soft.clone()).unwrap_or(""),
            "\n",
            "soft break range must be the newline, got {soft:?} {:?}",
            source.get(soft.clone())
        );

        let hard_src = "a  \nb\n";
        let hard = import(hard_src).blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::HardBreak { source_range, .. } => Some(source_range.clone()),
                _ => None,
            })
            .expect("hard break");
        let slice = hard_src.get(hard.clone()).unwrap_or("");
        assert!(
            slice.contains('\n') && slice.contains("  "),
            "hard break range must cover two-space marker and newline, got {hard:?} {slice:?}"
        );
        assert_ne!(hard.start, 0, "hard break must not start at the paragraph");
        assert_eq!(hard.start, 1, "two-space marker starts after `a`");

        let prev = import(hard_src).blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Run {
                    text, source_range, ..
                } if text == "a" => Some(source_range.clone()),
                _ => None,
            })
            .expect("run a");
        assert_eq!(
            prev.end, hard.start,
            "previous run must not overlap hard-break dest chrome, run={prev:?} hard={hard:?}"
        );
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
        assert!(all.iter().any(|k| matches!(k, BlockKind::ThematicBreak)));
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
        "Hello[^1]\n\n[^1]: the note\n",
        "Hello[^1] world\n",
        "Term\n\n: Definition\n",
        "[hello][ref]\n\n[ref]: https://e.com\n",
        "[ref]: https://e.com\n",
        "a <b>bold</b> and <br>break\n",
        "See[^1]\n\n[^1]: **bold** inside\n",
        "Term\n\n: **bold** details\n",
        "==highlight== and <mark>mark</mark>\n",
        "H~2~O and mc^2^ and H<sub>2</sub>O\n",
        "<div>\n**bold** inner\n</div>\n",
        "see $x^2$ and $$E=mc^2$$ costs $5\n",
        "A&amp;B and A&lt;C\n",
        "> [!NOTE]\n> alert body\n",
        "> [!TIP]\n> tip body\n",
        "> [!IMPORTANT]\n> important body\n",
        "> [!WARNING]\n> warning body\n",
        "> [!CAUTION]\n> caution body\n",
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
