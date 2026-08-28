// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use tree_sitter::Node;
use tree_sitter_md::{MarkdownCursor, MarkdownParser, MarkdownTree};

use crate::spans::{DelimiterSpan, SyntaxKind, SyntaxNodeSpan, TableRowKind};

/// Snapshot sent to the background parser thread.
#[derive(Debug, Clone)]
pub struct ParseSnapshot {
    pub revision: u64,
    pub text: String,
}

/// Result delivered after a background parse completes.
#[derive(Debug, Clone)]
pub struct ParseUpdate {
    pub revision: u64,
    pub spans: Vec<SyntaxNodeSpan>,
}

/// Extract syntax spans from Markdown source using tree-sitter-md.
pub fn extract_syntax_spans(source: &str) -> Vec<SyntaxNodeSpan> {
    let mut parser = MarkdownParser::default();
    let Some(tree) = parser.parse(source.as_bytes(), None) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    collect_spans(&mut cursor, source, &mut spans);
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    spans
}

fn collect_spans(cursor: &mut MarkdownCursor<'_>, source: &str, spans: &mut Vec<SyntaxNodeSpan>) {
    let node = cursor.node();
    if let Some((mut kind, table_row)) = map_syntax_kind(node.kind()) {
        if kind == SyntaxKind::List && extract_task_checked(node).is_some() {
            kind = SyntaxKind::TaskList;
        }
        let delimiter_spans = collect_delimiters(node);
        let language = if kind == SyntaxKind::CodeBlock {
            extract_code_language(node, source)
        } else {
            None
        };
        let task_checked = if kind == SyntaxKind::TaskList {
            extract_task_checked(node)
        } else {
            None
        };
        spans.push(SyntaxNodeSpan {
            kind,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            delimiter_spans,
            language,
            task_checked,
            table_row,
        });
    }

    if cursor.goto_first_child() {
        loop {
            collect_spans(cursor, source, spans);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn map_syntax_kind(kind: &str) -> Option<(SyntaxKind, Option<TableRowKind>)> {
    match kind {
        "atx_heading" | "setext_heading" => Some((SyntaxKind::Heading, None)),
        "strong_emphasis" => Some((SyntaxKind::Bold, None)),
        "emphasis" => Some((SyntaxKind::Italic, None)),
        "code_span" => Some((SyntaxKind::CodeInline, None)),
        "fenced_code_block" | "indented_code_block" => Some((SyntaxKind::CodeBlock, None)),
        "inline_link" | "full_link" | "reference_link" | "shortcut_link" | "link" => {
            Some((SyntaxKind::Link, None))
        }
        "inline_image" | "full_image" | "reference_image" | "shortcut_image" | "image" => {
            Some((SyntaxKind::Image, None))
        }
        "block_quote" => Some((SyntaxKind::BlockQuote, None)),
        "list" => Some((SyntaxKind::List, None)),
        "list_item" => Some((SyntaxKind::List, None)),
        "pipe_table" => Some((SyntaxKind::Table, None)),
        "pipe_table_header" => Some((SyntaxKind::Table, Some(TableRowKind::Header))),
        "pipe_table_delimiter_row" => Some((SyntaxKind::Table, Some(TableRowKind::Delimiter))),
        "pipe_table_row" => Some((SyntaxKind::Table, Some(TableRowKind::Body))),
        "strikethrough" => Some((SyntaxKind::Strikethrough, None)),
        "minus_metadata" | "plus_metadata" => Some((SyntaxKind::Frontmatter, None)),
        _ => None,
    }
}

fn extract_code_language(node: Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "info_string" {
            let raw = &source[child.start_byte()..child.end_byte()];
            let language = raw
                .split_whitespace()
                .next()
                .unwrap_or(raw)
                .trim();
            if language.is_empty() {
                return None;
            }
            return Some(language.to_ascii_lowercase());
        }
    }
    None
}

fn extract_task_checked(node: Node) -> Option<bool> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "task_list_marker_checked" => return Some(true),
            "task_list_marker_unchecked" => return Some(false),
            _ => {}
        }
    }
    None
}

fn collect_delimiters(node: Node) -> Vec<DelimiterSpan> {
    let mut delimiters = Vec::new();
    collect_delimiter_nodes(node, &mut delimiters);
    delimiters.sort_by_key(|span| span.start_byte);
    delimiters
}

fn collect_delimiter_nodes(node: Node, delimiters: &mut Vec<DelimiterSpan>) {
    if is_delimiter_node(node.kind()) {
        delimiters.push(DelimiterSpan::new(node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_delimiter_nodes(child, delimiters);
    }
}

fn is_delimiter_node(kind: &str) -> bool {
    matches!(
        kind,
        "emphasis_delimiter"
            | "code_span_delimiter"
            | "fenced_code_block_delimiter"
            | "block_quote_marker"
            | "list_marker"
            | "task_list_marker_checked"
            | "task_list_marker_unchecked"
            | "atx_h1_marker"
            | "atx_h2_marker"
            | "atx_h3_marker"
            | "atx_h4_marker"
            | "atx_h5_marker"
            | "atx_h6_marker"
            | "setext_h1_underline"
            | "setext_h2_underline"
    ) || matches!(kind, "[" | "]" | "(" | ")" | "`" | "*" | "_" | "|" | ">" | "!")
}

/// Dedicated background thread for Markdown parsing.
#[derive(Debug)]
pub struct BackgroundMarkdownParser {
    request_tx: Sender<ParseSnapshot>,
    update_rx: Receiver<ParseUpdate>,
    _worker: JoinHandle<()>,
}

impl BackgroundMarkdownParser {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();

        let worker = thread::Builder::new()
            .name("markrust-md-parser".into())
            .spawn(move || parser_worker_loop(request_rx, update_tx))
            .expect("failed to spawn markdown parser thread");

        Self {
            request_tx,
            update_rx,
            _worker: worker,
        }
    }

    pub fn request_parse(&self, snapshot: ParseSnapshot) {
        let _ = self.request_tx.send(snapshot);
    }

    pub fn poll_update(&self) -> Option<ParseUpdate> {
        self.update_rx.try_recv().ok()
    }

    pub fn drain_updates(&self) -> Vec<ParseUpdate> {
        let mut updates = Vec::new();
        while let Some(update) = self.poll_update() {
            updates.push(update);
        }
        updates
    }
}

impl Default for BackgroundMarkdownParser {
    fn default() -> Self {
        Self::new()
    }
}

fn parser_worker_loop(request_rx: Receiver<ParseSnapshot>, update_tx: Sender<ParseUpdate>) {
    let mut parser = MarkdownParser::default();

    while let Ok(snapshot) = request_rx.recv() {
        let spans = match parser.parse(snapshot.text.as_bytes(), None) {
            Some(tree) => extract_spans_from_tree(&tree, &snapshot.text),
            None => Vec::new(),
        };

        let _ = update_tx.send(ParseUpdate {
            revision: snapshot.revision,
            spans,
        });
    }
}

fn extract_spans_from_tree(tree: &MarkdownTree, source: &str) -> Vec<SyntaxNodeSpan> {
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    collect_spans(&mut cursor, source, &mut spans);
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spans::TableRowKind;

    #[test]
    fn extracts_bold_and_italic() {
        let spans = extract_syntax_spans("**bold** and *italic*");
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Bold));
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Italic));
        let bold = spans.iter().find(|s| s.kind == SyntaxKind::Bold).unwrap();
        assert_eq!(bold.delimiter_spans.len(), 4);
    }

    #[test]
    fn extracts_heading_and_code_block() {
        let spans = extract_syntax_spans("# Title\n\n```rust\nfn main() {}\n```");
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Heading));
        let code = spans
            .iter()
            .find(|s| s.kind == SyntaxKind::CodeBlock)
            .unwrap();
        assert_eq!(code.language.as_deref(), Some("rust"));
    }

    #[test]
    fn extracts_tables_tasks_blockquotes_links() {
        let source = "> quote\n\n| H | V |\n|---|---|\n| a | b |\n\n- [ ] open\n- [x] done\n\n[link](https://x)\n![alt](img.png)\n";
        let spans = extract_syntax_spans(source);
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::BlockQuote));
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Table));
        assert!(
            spans
                .iter()
                .any(|s| s.kind == SyntaxKind::Table && s.table_row == Some(TableRowKind::Header))
        );
        let tasks: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == SyntaxKind::TaskList)
            .collect();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.task_checked == Some(false)));
        assert!(tasks.iter().any(|t| t.task_checked == Some(true)));
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Link));
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Image));
    }

    #[test]
    fn extracts_frontmatter() {
        let spans = extract_syntax_spans("---\ntitle: X\n---\n\n# Hi");
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Frontmatter));
    }

    #[test]
    fn background_parser_returns_update() {
        let parser = BackgroundMarkdownParser::new();
        parser.request_parse(ParseSnapshot {
            revision: 1,
            text: "**test**".into(),
        });

        for _ in 0..100 {
            if let Some(update) = parser.poll_update() {
                assert_eq!(update.revision, 1);
                assert!(!update.spans.is_empty());
                return;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for parse update");
    }
}
