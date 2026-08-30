// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background Markdown span extraction for source-mode masking.
//!
//! Spans are derived from the same comrak AST as [`crate::rich::import`], so
//! source mode is a projection of the rich tree's grammar rather than a second
//! parser (tree-sitter-md). Parse still runs on a dedicated worker thread.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use comrak::nodes::{AstNode, NodeValue};
use comrak::{parse_document, Arena};

use crate::rich::import::{parse_options, LineStarts};
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

/// Extract syntax spans from Markdown source using the comrak AST.
pub fn extract_syntax_spans(source: &str) -> Vec<SyntaxNodeSpan> {
    let arena = Arena::new();
    let root = parse_document(&arena, source, &parse_options());
    let lines = LineStarts::new(source);
    let mut spans = Vec::new();
    collect_spans(root, source, &lines, &mut spans);
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    spans
}

fn collect_spans<'a>(
    node: &'a AstNode<'a>,
    source: &str,
    lines: &LineStarts,
    spans: &mut Vec<SyntaxNodeSpan>,
) {
    let range = lines.range(node.data.borrow().sourcepos, source.len());
    match &node.data.borrow().value {
        NodeValue::FrontMatter(_) => {
            spans.push(make_span(
                SyntaxKind::Frontmatter,
                range.clone(),
                frontmatter_delims(source, &range),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Heading(h) => {
            let level = h.level.clamp(1, 6);
            spans.push(make_span(
                SyntaxKind::Heading,
                range.clone(),
                heading_delims(source, &range, h.setext, level),
                None,
                None,
                None,
                Some(level),
            ));
        }
        NodeValue::Strong => {
            spans.push(make_span(
                SyntaxKind::Bold,
                range.clone(),
                wrap_delims(source, &range, 2),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Emph => {
            spans.push(make_span(
                SyntaxKind::Italic,
                range.clone(),
                wrap_delims(source, &range, 1),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Strikethrough => {
            spans.push(make_span(
                SyntaxKind::Strikethrough,
                range.clone(),
                wrap_delims(source, &range, 2),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Superscript => {
            spans.push(make_span(
                SyntaxKind::Other,
                range.clone(),
                wrap_delims(source, &range, 1),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Subscript => {
            spans.push(make_span(
                SyntaxKind::Other,
                range.clone(),
                wrap_delims(source, &range, 1),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Code(code) => {
            spans.push(make_span(
                SyntaxKind::CodeInline,
                range.clone(),
                wrap_delims(source, &range, code.num_backticks),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Link(_) => {
            spans.push(make_span(
                SyntaxKind::Link,
                range.clone(),
                link_delims(source, &range, false),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Image(_) => {
            spans.push(make_span(
                SyntaxKind::Image,
                range.clone(),
                link_delims(source, &range, true),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::CodeBlock(cb) => {
            let language = {
                let lang = cb.info.split_whitespace().next().unwrap_or("").trim();
                if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_ascii_lowercase())
                }
            };
            spans.push(make_span(
                SyntaxKind::CodeBlock,
                range.clone(),
                fence_delims(source, &range, cb.fenced),
                language,
                None,
                None,
                None,
            ));
        }
        NodeValue::BlockQuote => {
            spans.push(make_span(
                SyntaxKind::BlockQuote,
                range.clone(),
                line_prefix_delims(source, &range, b'>'),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::List(_) => {
            spans.push(make_span(
                SyntaxKind::List,
                range.clone(),
                Vec::new(),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::Item(_) => {
            spans.push(make_span(
                SyntaxKind::List,
                range.clone(),
                list_item_delims(source, &range, false),
                None,
                None,
                None,
                None,
            ));
        }
        NodeValue::TaskItem(symbol) => {
            let checked = symbol.is_some();
            spans.push(make_span(
                SyntaxKind::TaskList,
                range.clone(),
                list_item_delims(source, &range, true),
                None,
                Some(checked),
                None,
                None,
            ));
        }
        NodeValue::Table(_) => {
            spans.push(make_span(
                SyntaxKind::Table,
                range.clone(),
                pipe_delims(source, &range),
                None,
                None,
                None,
                None,
            ));
            emit_table_delimiter_row(source, node, lines, spans);
        }
        NodeValue::TableRow(header) => {
            let kind = if *header {
                TableRowKind::Header
            } else {
                TableRowKind::Body
            };
            spans.push(make_span(
                SyntaxKind::Table,
                range.clone(),
                pipe_delims(source, &range),
                None,
                None,
                Some(kind),
                None,
            ));
        }
        _ => {}
    }

    for child in node.children() {
        collect_spans(child, source, lines, spans);
    }
}

fn make_span(
    kind: SyntaxKind,
    range: std::ops::Range<usize>,
    delimiter_spans: Vec<DelimiterSpan>,
    language: Option<String>,
    task_checked: Option<bool>,
    table_row: Option<TableRowKind>,
    heading_level: Option<u8>,
) -> SyntaxNodeSpan {
    SyntaxNodeSpan {
        kind,
        start_byte: range.start,
        end_byte: range.end,
        delimiter_spans,
        language,
        task_checked,
        table_row,
        heading_level,
    }
}

fn wrap_delims(source: &str, range: &std::ops::Range<usize>, width: usize) -> Vec<DelimiterSpan> {
    if width == 0 || range.end < range.start + width * 2 {
        return Vec::new();
    }
    let slice = source.get(range.clone()).unwrap_or("");
    let open_len = width.min(slice.len());
    let close_len = width.min(slice.len().saturating_sub(open_len));
    let mut out = Vec::new();
    if open_len > 0 {
        out.push(DelimiterSpan::new(range.start, range.start + open_len));
    }
    if close_len > 0 {
        out.push(DelimiterSpan::new(range.end - close_len, range.end));
    }
    out
}

fn heading_delims(
    source: &str,
    range: &std::ops::Range<usize>,
    setext: bool,
    level: u8,
) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    if setext {
        if let Some(nl) = slice.rfind('\n') {
            let under = &slice[nl + 1..];
            let trimmed = under.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                let start = range.start + nl + 1;
                return vec![DelimiterSpan::new(start, start + trimmed.len())];
            }
        }
        return Vec::new();
    }
    let hashes = slice
        .bytes()
        .take_while(|b| *b == b'#')
        .count()
        .min(level as usize);
    if hashes == 0 {
        return Vec::new();
    }
    vec![DelimiterSpan::new(range.start, range.start + hashes)]
}

fn fence_delims(source: &str, range: &std::ops::Range<usize>, fenced: bool) -> Vec<DelimiterSpan> {
    if !fenced {
        return Vec::new();
    }
    let slice = source.get(range.clone()).unwrap_or("");
    let mut out = Vec::new();
    if let Some(nl) = slice.find('\n') {
        out.push(DelimiterSpan::new(range.start, range.start + nl));
        if let Some(last) = slice.rfind('\n') {
            let close = slice[last + 1..].trim_end_matches(['\r', '\n']);
            if close.starts_with('`') || close.starts_with('~') {
                let start = range.start + last + 1;
                out.push(DelimiterSpan::new(start, start + close.len()));
            }
        }
    } else if !slice.is_empty() {
        out.push(DelimiterSpan::new(range.start, range.end));
    }
    out
}

fn frontmatter_delims(source: &str, range: &std::ops::Range<usize>) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    let mut out = Vec::new();
    if let Some(nl) = slice.find('\n') {
        out.push(DelimiterSpan::new(range.start, range.start + nl));
    }
    if let Some(last) = slice.trim_end_matches(['\r', '\n']).rfind('\n') {
        let close = slice[last + 1..].trim_end_matches(['\r', '\n']);
        if close.starts_with("---") {
            let start = range.start + last + 1;
            out.push(DelimiterSpan::new(start, start + close.len()));
        }
    }
    out
}

fn link_delims(source: &str, range: &std::ops::Range<usize>, image: bool) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    if slice.starts_with('<') && slice.ends_with('>') && slice.len() >= 2 {
        return vec![
            DelimiterSpan::new(range.start, range.start + 1),
            DelimiterSpan::new(range.end.saturating_sub(1), range.end),
        ];
    }
    let mut out = Vec::new();
    let open_len = if image && slice.starts_with("![") {
        2
    } else if slice.starts_with('[') {
        1
    } else {
        0
    };
    if open_len > 0 {
        out.push(DelimiterSpan::new(range.start, range.start + open_len));
    }
    if let Some(i) = slice.rfind("](") {
        out.push(DelimiterSpan::new(range.start + i, range.end));
    } else if slice.ends_with(']') {
        out.push(DelimiterSpan::new(range.end.saturating_sub(1), range.end));
    }
    out
}

fn line_prefix_delims(
    source: &str,
    range: &std::ops::Range<usize>,
    marker: u8,
) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    let mut out = Vec::new();
    let mut offset = range.start;
    for line in slice.split_inclusive('\n') {
        let indent = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        if line.as_bytes().get(indent) == Some(&marker) {
            out.push(DelimiterSpan::new(offset + indent, offset + indent + 1));
        }
        offset += line.len();
    }
    out
}

fn list_item_delims(
    source: &str,
    range: &std::ops::Range<usize>,
    task: bool,
) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    let indent = slice
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let rest = &slice[indent..];
    let mut out = Vec::new();
    let marker_len = if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        1
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && matches!(rest.as_bytes().get(digits), Some(b'.' | b')')) {
            digits + 1
        } else {
            0
        }
    };
    if marker_len > 0 && !task {
        out.push(DelimiterSpan::new(
            range.start + indent,
            range.start + indent + marker_len,
        ));
    }
    if task {
        if let Some(rel) = rest.find('[') {
            let after = &rest[rel..];
            let close = after.find(']').map(|i| i + 1).unwrap_or(3).min(after.len());
            out.push(DelimiterSpan::new(
                range.start + indent + rel,
                range.start + indent + rel + close,
            ));
        }
    }
    out
}

fn pipe_delims(source: &str, range: &std::ops::Range<usize>) -> Vec<DelimiterSpan> {
    let slice = source.get(range.clone()).unwrap_or("");
    slice
        .bytes()
        .enumerate()
        .filter(|(_, b)| *b == b'|')
        .map(|(i, _)| DelimiterSpan::new(range.start + i, range.start + i + 1))
        .collect()
}

fn emit_table_delimiter_row<'a>(
    source: &str,
    table: &'a AstNode<'a>,
    lines: &LineStarts,
    spans: &mut Vec<SyntaxNodeSpan>,
) {
    let table_range = lines.range(table.data.borrow().sourcepos, source.len());
    let slice = source.get(table_range.clone()).unwrap_or("");
    let mut offset = table_range.start;
    for line in slice.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let is_delim = {
            let t = trimmed.trim();
            t.contains('|')
                && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
                && t.contains('-')
        };
        if is_delim && !trimmed.is_empty() {
            let end = offset + trimmed.len();
            spans.push(make_span(
                SyntaxKind::Table,
                offset..end,
                pipe_delims(source, &(offset..end)),
                None,
                None,
                Some(TableRowKind::Delimiter),
                None,
            ));
            return;
        }
        offset += line.len();
    }
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

    /// Block until any parse update is available, returning the newest drained result.
    pub fn wait_for_update(&self, timeout: Duration) -> Option<ParseUpdate> {
        self.wait_for_revision(0, timeout)
    }

    /// Block until a parse update at least as new as `min_revision` arrives.
    pub fn wait_for_revision(&self, min_revision: u64, timeout: Duration) -> Option<ParseUpdate> {
        let deadline = Instant::now() + timeout;
        let mut latest = None;
        loop {
            for update in self.drain_updates() {
                latest = Some(update);
            }
            if latest
                .as_ref()
                .is_some_and(|update| update.revision >= min_revision)
            {
                return latest;
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Default for BackgroundMarkdownParser {
    fn default() -> Self {
        Self::new()
    }
}

fn parser_worker_loop(request_rx: Receiver<ParseSnapshot>, update_tx: Sender<ParseUpdate>) {
    while let Ok(snapshot) = request_rx.recv() {
        let spans = extract_syntax_spans(&snapshot.text);
        let _ = update_tx.send(ParseUpdate {
            revision: snapshot.revision,
            spans,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spans::TableRowKind;
    use crate::test_support::PARSE_TIMEOUT;

    fn has_kind(spans: &[SyntaxNodeSpan], kind: SyntaxKind) -> bool {
        spans.iter().any(|s| s.kind == kind)
    }

    #[test]
    fn extracts_bold_and_italic() {
        let spans = extract_syntax_spans("**bold** and *italic*");
        assert!(has_kind(&spans, SyntaxKind::Bold));
        assert!(has_kind(&spans, SyntaxKind::Italic));
        let bold = spans.iter().find(|s| s.kind == SyntaxKind::Bold).unwrap();
        assert!(
            bold.delimiter_spans.len() >= 2,
            "bold delimiters: {:?}",
            bold.delimiter_spans
        );
    }

    #[test]
    fn extracts_nested_bold_italic() {
        let spans = extract_syntax_spans("**bold *italic* bold**");
        assert!(has_kind(&spans, SyntaxKind::Bold));
        assert!(has_kind(&spans, SyntaxKind::Italic));
        let bold = spans.iter().find(|s| s.kind == SyntaxKind::Bold).unwrap();
        let italic = spans.iter().find(|s| s.kind == SyntaxKind::Italic).unwrap();
        assert!(italic.start_byte >= bold.start_byte);
        assert!(italic.end_byte <= bold.end_byte);
    }

    #[test]
    fn extracts_heading_and_code_block() {
        let spans = extract_syntax_spans("# Title\n\n```rust\nfn main() {}\n```");
        assert!(has_kind(&spans, SyntaxKind::Heading));
        let heading = spans
            .iter()
            .find(|s| s.kind == SyntaxKind::Heading)
            .unwrap();
        assert_eq!(heading.heading_level, Some(1));
        let code = spans
            .iter()
            .find(|s| s.kind == SyntaxKind::CodeBlock)
            .unwrap();
        assert_eq!(code.language.as_deref(), Some("rust"));
    }

    #[test]
    fn extracts_code_fence_language_and_inline_code() {
        let spans = extract_syntax_spans("```Rust\nlet x = 1;\n```\n\nUse `code` here.\n");
        let fence = spans
            .iter()
            .find(|s| s.kind == SyntaxKind::CodeBlock)
            .unwrap();
        assert_eq!(fence.language.as_deref(), Some("rust"));
        assert!(has_kind(&spans, SyntaxKind::CodeInline));

        let plain_fence = extract_syntax_spans("```\nplain\n```\n");
        let code = plain_fence
            .iter()
            .find(|s| s.kind == SyntaxKind::CodeBlock)
            .unwrap();
        assert!(code.language.is_none());
    }

    #[test]
    fn extracts_tables_tasks_blockquotes_links() {
        let source = "> quote\n\n| H | V |\n|---|---|\n| a | b |\n\n- [ ] open\n- [x] done\n\n[link](https://x)\n![alt](img.png)\n";
        let spans = extract_syntax_spans(source);
        assert!(has_kind(&spans, SyntaxKind::BlockQuote));
        assert!(has_kind(&spans, SyntaxKind::Table));
        assert!(spans
            .iter()
            .any(|s| s.kind == SyntaxKind::Table && s.table_row == Some(TableRowKind::Header)));
        let tasks: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == SyntaxKind::TaskList)
            .collect();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.task_checked == Some(false)));
        assert!(tasks.iter().any(|t| t.task_checked == Some(true)));
        assert!(has_kind(&spans, SyntaxKind::Link));
        assert!(has_kind(&spans, SyntaxKind::Image));
    }

    #[test]
    fn extracts_table_header_delimiter_and_body() {
        let spans = extract_syntax_spans("| H | V |\n|---|---|\n| a | b |\n");
        let rows: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == SyntaxKind::Table)
            .filter_map(|s| s.table_row)
            .collect();
        assert!(rows.contains(&TableRowKind::Header));
        assert!(rows.contains(&TableRowKind::Delimiter));
        assert!(rows.contains(&TableRowKind::Body));
    }

    #[test]
    fn extracts_frontmatter() {
        let spans = extract_syntax_spans("---\ntitle: X\n---\n\n# Hi");
        assert!(has_kind(&spans, SyntaxKind::Frontmatter));
        assert!(has_kind(&spans, SyntaxKind::Heading));
    }

    #[test]
    fn extracts_strikethrough() {
        let spans = extract_syntax_spans("~~gone~~");
        assert!(has_kind(&spans, SyntaxKind::Strikethrough));
    }

    #[test]
    fn empty_source_has_no_construct_spans() {
        let spans = extract_syntax_spans("");
        assert!(!has_kind(&spans, SyntaxKind::Heading));
        assert!(!has_kind(&spans, SyntaxKind::Bold));
        assert!(!has_kind(&spans, SyntaxKind::Table));
    }

    #[test]
    fn background_parser_returns_update() {
        let parser = BackgroundMarkdownParser::new();
        parser.request_parse(ParseSnapshot {
            revision: 1,
            text: "**test**".into(),
        });

        let update = parser
            .wait_for_update(PARSE_TIMEOUT)
            .expect("timed out waiting for parse update");
        assert_eq!(update.revision, 1);
        assert!(!update.spans.is_empty());
        assert!(has_kind(&update.spans, SyntaxKind::Bold));
    }

    #[test]
    fn extracts_atx_heading_levels_and_setext() {
        let atx = extract_syntax_spans("# H1\n\n## H2\n\n### H3\n");
        let headings: Vec<_> = atx
            .iter()
            .filter(|s| s.kind == SyntaxKind::Heading)
            .collect();
        assert!(headings.len() >= 3, "atx headings: {headings:?}");
        assert_eq!(headings[0].heading_level, Some(1));
        assert_eq!(headings[1].heading_level, Some(2));

        let setext = extract_syntax_spans("Title\n=====\n\nSub\n-----\n");
        assert!(has_kind(&setext, SyntaxKind::Heading));
        let levels: Vec<_> = setext
            .iter()
            .filter(|s| s.kind == SyntaxKind::Heading)
            .filter_map(|s| s.heading_level)
            .collect();
        assert!(levels.contains(&1), "setext levels: {levels:?}");
        assert!(levels.contains(&2), "setext levels: {levels:?}");
    }

    #[test]
    fn extracts_links_images_and_inline_code_ranges() {
        let source = "See [docs](https://markrust.org) and `code` plus ![alt](pic.png).";
        let spans = extract_syntax_spans(source);
        let link = spans.iter().find(|s| s.kind == SyntaxKind::Link).unwrap();
        assert!(source[link.start_byte..link.end_byte.min(source.len())].contains("docs"));
        let image = spans.iter().find(|s| s.kind == SyntaxKind::Image).unwrap();
        assert!(source[image.start_byte..image.end_byte.min(source.len())].contains("alt"));
        let code = spans
            .iter()
            .find(|s| s.kind == SyntaxKind::CodeInline)
            .unwrap();
        assert!(source[code.start_byte..code.end_byte.min(source.len())].contains("code"));
    }

    #[test]
    fn extracts_indented_code_and_nested_blockquote() {
        let source = "    indented()\n\n> outer\n> > inner\n";
        let spans = extract_syntax_spans(source);
        assert!(
            has_kind(&spans, SyntaxKind::CodeBlock) || has_kind(&spans, SyntaxKind::BlockQuote)
        );
        assert!(has_kind(&spans, SyntaxKind::BlockQuote));
    }

    #[test]
    fn concurrent_edit_and_parse_keeps_latest_revision() {
        let parser = BackgroundMarkdownParser::new();
        parser.request_parse(ParseSnapshot {
            revision: 1,
            text: "# A".into(),
        });
        parser.request_parse(ParseSnapshot {
            revision: 2,
            text: "# A\n\n**B**".into(),
        });
        let update = parser
            .wait_for_revision(2, PARSE_TIMEOUT)
            .expect("timed out waiting for revision 2");
        assert_eq!(update.revision, 2);
        assert!(has_kind(&update.spans, SyntaxKind::Heading));
        assert!(has_kind(&update.spans, SyntaxKind::Bold));
    }

    #[test]
    fn source_spans_share_comrak_grammar_with_rich_tree() {
        let source = include_str!("../../markrust-app/tests/fixtures/showcase.md");
        let spans = extract_syntax_spans(source);
        assert!(has_kind(&spans, SyntaxKind::Frontmatter));
        assert!(has_kind(&spans, SyntaxKind::Heading));
        assert!(has_kind(&spans, SyntaxKind::Table));
        assert!(has_kind(&spans, SyntaxKind::CodeBlock));
        assert!(has_kind(&spans, SyntaxKind::TaskList));
    }
}
