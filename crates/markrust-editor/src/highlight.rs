// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use tree_sitter::{Language, Query, QueryCursor, StreamingIterator, Tree};

/// Token category for syntax highlighting inside fenced code blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
    Property,
    Punctuation,
    Plain,
}

/// Absolute byte range in the document with a highlight category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: HighlightKind,
}

/// Highlight code block contents using tree-sitter when a language tag is known.
pub fn highlight_code_block(language: &str, code: &str, base_offset: usize) -> Vec<HighlightSpan> {
    let Some(lang) = language_for_tag(language) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(code, None) else {
        return Vec::new();
    };
    highlight_tree(&tree, code, base_offset, language)
}

fn language_for_tag(tag: &str) -> Option<Language> {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        "yaml" | "yml" => Some(tree_sitter_yaml::LANGUAGE.into()),
        "bash" | "sh" | "shell" | "zsh" => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

fn highlight_tree(
    tree: &Tree,
    source: &str,
    base_offset: usize,
    language: &str,
) -> Vec<HighlightSpan> {
    let query_src = query_for_language(language);
    let Ok(query) = Query::new(&language_for_tag(language).unwrap(), query_src) else {
        return fallback_highlight(tree, base_offset);
    };

    let mut cursor = QueryCursor::new();
    let mut spans = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(mat) = matches.next() {
        for capture in mat.captures {
            let name = query.capture_names()[capture.index as usize];
            let kind = map_capture(name);
            if kind == HighlightKind::Plain {
                continue;
            }
            let node = capture.node;
            spans.push(HighlightSpan {
                start_byte: base_offset + node.start_byte(),
                end_byte: base_offset + node.end_byte(),
                kind,
            });
        }
    }
    if spans.is_empty() {
        return fallback_highlight(tree, base_offset);
    }
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    spans
}

fn fallback_highlight(tree: &Tree, base_offset: usize) -> Vec<HighlightSpan> {
    let mut spans = Vec::new();
    collect_named_nodes(tree.root_node(), base_offset, &mut spans);
    spans
}

fn collect_named_nodes(
    node: tree_sitter::Node,
    base_offset: usize,
    spans: &mut Vec<HighlightSpan>,
) {
    if node.is_named() && node.start_byte() < node.end_byte() {
        spans.push(HighlightSpan {
            start_byte: base_offset + node.start_byte(),
            end_byte: base_offset + node.end_byte(),
            kind: map_node_kind(node.kind()),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_named_nodes(child, base_offset, spans);
    }
}

fn map_capture(name: &str) -> HighlightKind {
    if name.contains("keyword") {
        HighlightKind::Keyword
    } else if name.contains("string") {
        HighlightKind::String
    } else if name.contains("number") || name.contains("integer") || name.contains("float") {
        HighlightKind::Number
    } else if name.contains("comment") {
        HighlightKind::Comment
    } else if name.contains("function") || name.contains("method") {
        HighlightKind::Function
    } else if name.contains("type") || name.contains("class") {
        HighlightKind::Type
    } else if name.contains("property") || name.contains("field") {
        HighlightKind::Property
    } else if name.contains("punctuation") {
        HighlightKind::Punctuation
    } else {
        HighlightKind::Plain
    }
}

fn map_node_kind(kind: &str) -> HighlightKind {
    match kind {
        "identifier" | "type_identifier" | "primitive_type" | "field_identifier" => {
            HighlightKind::Type
        }
        "string" | "string_content" | "string_literal" | "interpreted_string_literal" => {
            HighlightKind::String
        }
        "number" | "integer" | "float" => HighlightKind::Number,
        "comment" | "line_comment" | "block_comment" => HighlightKind::Comment,
        "function_item" | "call_expression" | "command" => HighlightKind::Function,
        _ if kind.contains("keyword") => HighlightKind::Keyword,
        _ => HighlightKind::Plain,
    }
}

fn query_for_language(language: &str) -> &'static str {
    match language.to_ascii_lowercase().as_str() {
        "rust" | "rs" => tree_sitter_rust::HIGHLIGHTS_QUERY,
        "json" => tree_sitter_json::HIGHLIGHTS_QUERY,
        "yaml" | "yml" => tree_sitter_yaml::HIGHLIGHTS_QUERY,
        "bash" | "sh" | "shell" | "zsh" => tree_sitter_bash::HIGHLIGHT_QUERY,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_keywords() {
        let code = "fn main() {}";
        let spans = highlight_code_block("rust", code, 0);
        assert!(spans.iter().any(|s| s.kind == HighlightKind::Keyword));
    }

    #[test]
    fn highlights_json_strings() {
        let code = r#"{ "key": "value" }"#;
        let spans = highlight_code_block("json", code, 0);
        assert!(spans.iter().any(|s| s.kind == HighlightKind::String));
    }

    #[test]
    fn unknown_language_returns_empty() {
        assert!(highlight_code_block("brainfuck", "++++", 0).is_empty());
    }

    #[test]
    fn highlights_yaml_keys() {
        let spans = highlight_code_block("yaml", "name: MarkRust\ncount: 1\n", 0);
        assert!(!spans.is_empty(), "yaml highlighter should emit spans");
    }

    #[test]
    fn highlights_bash_commands() {
        let spans = highlight_code_block("bash", "echo hello\n", 0);
        assert!(!spans.is_empty(), "bash highlighter should emit spans");
    }

    #[test]
    fn empty_fence_returns_no_or_empty_spans() {
        assert!(highlight_code_block("rust", "", 0).is_empty());
        assert!(highlight_code_block("json", "", 0).is_empty());
    }

    #[test]
    fn empty_language_tag_falls_back() {
        assert!(highlight_code_block("", "fn main() {}", 0).is_empty());
        assert!(highlight_code_block("unknown-lang", "fn main() {}", 0).is_empty());
    }
}
