// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

use yaml_rust::{Yaml, YamlLoader};

/// Why a frontmatter edit was rejected before it touched the document.
///
/// The rich frontmatter controls deliberately support a small, predictable
/// profile: one YAML mapping, with no YAML document markers inside its body.
/// This matches what Markdown frontmatter consumers expect and prevents a
/// pasted `---` or malformed scalar from turning the Markdown body into YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontmatterError {
    /// The YAML parser could not read the proposed body.
    InvalidYaml,
    /// Frontmatter must be one mapping, not a YAML stream, list, or scalar.
    InvalidMapping,
    /// A `---` / `...` marker appeared where the inner YAML body is expected.
    UnexpectedDelimiter,
    /// A field key cannot be represented safely as a top-level plain key.
    InvalidKey,
}

impl FrontmatterError {
    /// Short text suitable for an inline editor error message.
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidYaml => "Frontmatter must be valid YAML.",
            Self::InvalidMapping => "Frontmatter must be a single YAML mapping.",
            Self::UnexpectedDelimiter => {
                "Edit the YAML body without --- or ... document delimiters."
            }
            Self::InvalidKey => "Frontmatter keys may use letters, digits, _ and -.",
        }
    }
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for FrontmatterError {}

/// Parsed YAML frontmatter metadata for UI hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Option<String>,
    /// Inner YAML between the opening `---` and the closing `---` / `...`
    /// (no fence lines).
    pub yaml_body: String,
}

/// Detect YAML frontmatter at the document start. Jekyll/Pandoc open with
/// `---` and close with `---` or YAML document-end `...`. Mid-document `...`
/// is not frontmatter.
pub fn parse_frontmatter(source: &str) -> Option<FrontmatterInfo> {
    let trimmed = source.strip_prefix('\u{feff}').unwrap_or(source);
    if !trimmed.starts_with("---") {
        return None;
    }
    let start = source.len() - trimmed.len();
    let after_dashes = &trimmed[3..];
    let (newline_len, yaml_body) = if let Some(rest) = after_dashes.strip_prefix("\r\n") {
        (2, rest)
    } else if let Some(rest) = after_dashes.strip_prefix('\n') {
        (1, rest)
    } else {
        return None;
    };
    let close = find_closing_fence(yaml_body)?;
    let yaml = &yaml_body[..close.start];
    let mut end = start + 3 + newline_len + close.end;
    let rest = &yaml_body[close.end..];
    if rest.starts_with("\r\n") {
        end += 2;
    } else if rest.starts_with('\n') {
        end += 1;
    }

    Some(FrontmatterInfo {
        start_byte: start,
        end_byte: end,
        title: extract_yaml_scalar_key(yaml, "title"),
        description: extract_yaml_scalar_key(yaml, "description"),
        tags: extract_yaml_scalar_key(yaml, "tags"),
        yaml_body: yaml.to_string(),
    })
}

/// Validate an *inner* YAML frontmatter body before it is written to the
/// document. The frontmatter editor owns the surrounding `---` / `...`
/// fences, so document delimiters inside the body are rejected rather than
/// interpreted as a second YAML document.
pub fn validate_frontmatter_yaml(raw: &str) -> Result<(), FrontmatterError> {
    let body = raw.trim();
    if body.is_empty() {
        return Ok(());
    }
    if body.lines().any(is_document_delimiter_line) {
        return Err(FrontmatterError::UnexpectedDelimiter);
    }

    let documents = YamlLoader::load_from_str(body).map_err(|_| FrontmatterError::InvalidYaml)?;
    if documents.len() != 1 {
        return Err(FrontmatterError::InvalidMapping);
    }
    match documents.into_iter().next() {
        // yaml-rust represents a comments-only document as BadValue. It is
        // still valid to retain it and add a first frontmatter field later.
        Some(Yaml::Hash(_)) | Some(Yaml::BadValue) => Ok(()),
        _ => Err(FrontmatterError::InvalidMapping),
    }
}

/// Normalize either an inner YAML body or a complete frontmatter block into
/// the fenced representation stored in Markdown. Invalid input is rejected
/// before any document splice is attempted.
pub fn normalize_frontmatter(raw: &str) -> Result<String, FrontmatterError> {
    let input = parse_frontmatter_input(raw)?;
    validate_frontmatter_yaml(&input.body)?;
    Ok(render_fenced_frontmatter(&input.body, input.closer))
}

/// Insert, replace, or remove a top-level frontmatter field safely.
///
/// Field overlay text is never concatenated as YAML source: title and
/// description use a quoted YAML scalar, tags become a quoted flow sequence,
/// and all other safe plain keys use a quoted scalar. Empty values preserve
/// the existing UI contract of removing that key. The old and new body are
/// both validated, so an invalid source is left unchanged rather than being
/// "fixed" into a subtly different document.
pub fn upsert_yaml_key(raw: &str, key: &str, value: &str) -> Result<String, FrontmatterError> {
    if !is_safe_field_key(key) {
        return Err(FrontmatterError::InvalidKey);
    }
    let input = parse_frontmatter_input(raw)?;
    validate_frontmatter_yaml(&input.body)?;

    let rendered = serialize_yaml_field(key, value);
    let updated = replace_top_level_yaml_field(&input.body, key, rendered.as_deref());
    validate_frontmatter_yaml(&updated)?;

    if updated.trim().is_empty() {
        return Ok(String::new());
    }
    if input.had_fences || raw.trim().is_empty() {
        Ok(render_fenced_frontmatter(&updated, input.closer))
    } else {
        Ok(format!("{}\n", updated.trim()))
    }
}

/// Comrak's `front_matter_delimiter` is a single string (`---`). Jekyll/Pandoc
/// `...` closers are rewritten to `---` for parse only (same length, so
/// sourcepos still maps onto the original buffer).
pub(crate) fn comrak_parse_input(source: &str) -> Cow<'_, str> {
    let Some(range) = ellipsis_frontmatter_closer_range(source) else {
        return Cow::Borrowed(source);
    };
    let mut copy = source.to_string();
    copy.replace_range(range, "---");
    Cow::Owned(copy)
}

fn ellipsis_frontmatter_closer_range(source: &str) -> Option<Range<usize>> {
    let info = parse_frontmatter(source)?;
    let mut end = info.end_byte;
    let bytes = source.as_bytes();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && bytes[end - 1] == b'\r' {
        end -= 1;
    }
    if end >= 3 && source.get(end - 3..end) == Some("...") {
        Some(end - 3..end)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
struct FrontmatterInput {
    body: String,
    had_fences: bool,
    closer: &'static str,
}

/// Interpret a raw frontmatter edit without ever treating an incomplete
/// opening fence as ordinary YAML text. `SetFrontmatter` accepts full fenced
/// blocks for API callers, while the WYSIWYG YAML overlay supplies only the
/// body.
fn parse_frontmatter_input(raw: &str) -> Result<FrontmatterInput, FrontmatterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(FrontmatterInput {
            body: String::new(),
            had_fences: false,
            closer: "---",
        });
    }

    if trimmed.starts_with("---") {
        let info = parse_frontmatter(trimmed).ok_or(FrontmatterError::UnexpectedDelimiter)?;
        if info.start_byte != 0
            || trimmed
                .get(info.end_byte..)
                .is_none_or(|suffix| !suffix.trim().is_empty())
        {
            return Err(FrontmatterError::UnexpectedDelimiter);
        }
        let before_closer = trimmed.get(..info.end_byte).unwrap_or(trimmed).trim_end();
        return Ok(FrontmatterInput {
            body: info.yaml_body.trim().to_string(),
            had_fences: true,
            closer: if before_closer.ends_with("...") {
                "..."
            } else {
                "---"
            },
        });
    }

    if trimmed.lines().any(is_document_delimiter_line) {
        return Err(FrontmatterError::UnexpectedDelimiter);
    }
    Ok(FrontmatterInput {
        body: trimmed.to_string(),
        had_fences: false,
        closer: "---",
    })
}

fn render_fenced_frontmatter(body: &str, closer: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        String::new()
    } else {
        format!("---\n{body}\n{closer}\n")
    }
}

fn serialize_yaml_field(key: &str, value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    if key == "tags" {
        let tags = parse_tag_draft(value);
        if tags.is_empty() {
            return None;
        }
        let tags = tags
            .iter()
            .map(|tag| yaml_quoted_scalar(tag))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("{key}: [{tags}]"));
    }
    Some(format!("{key}: {}", yaml_quoted_scalar(value)))
}

/// Tags are a small convenience field, not a raw YAML escape hatch. Accept
/// the historical `[a, b]` form as well as a friendlier `a, b` draft, then
/// emit every tag as a quoted scalar.
fn parse_tag_draft(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Ok(documents) = YamlLoader::load_from_str(trimmed) {
            if let [Yaml::Array(items)] = documents.as_slice() {
                let parsed = items.iter().map(yaml_tag_text).collect::<Option<Vec<_>>>();
                if let Some(tags) = parsed {
                    return tags.into_iter().filter(|tag| !tag.is_empty()).collect();
                }
            }
        }
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

fn yaml_tag_text(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Integer(number) => Some(number.to_string()),
        Yaml::Real(number) => Some(number.clone()),
        Yaml::Boolean(value) => Some(value.to_string()),
        Yaml::Null => Some("null".to_string()),
        // A tag must be a scalar. Falling back to the comma-separated input
        // path keeps odd pasted input harmless without accepting nested YAML.
        Yaml::Array(_) | Yaml::Hash(_) | Yaml::Alias(_) | Yaml::BadValue => None,
    }
}

fn yaml_quoted_scalar(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push('"');
    for ch in value.chars() {
        match ch {
            '"' => rendered.push_str("\\\""),
            '\\' => rendered.push_str("\\\\"),
            '\u{08}' => rendered.push_str("\\b"),
            '\t' => rendered.push_str("\\t"),
            '\n' => rendered.push_str("\\n"),
            '\u{0c}' => rendered.push_str("\\f"),
            '\r' => rendered.push_str("\\r"),
            ch if ch.is_control() => {
                let code = ch as u32;
                if code <= 0xffff {
                    rendered.push_str(&format!("\\u{code:04X}"));
                } else {
                    rendered.push_str(&format!("\\U{code:08X}"));
                }
            }
            ch => rendered.push(ch),
        }
    }
    rendered.push('"');
    rendered
}

fn replace_top_level_yaml_field(body: &str, key: &str, replacement: Option<&str>) -> String {
    let lines = body.lines().collect::<Vec<_>>();
    let mut updated = Vec::with_capacity(lines.len() + usize::from(replacement.is_some()));
    let mut inserted = false;
    let mut index = 0;
    while index < lines.len() {
        if is_top_level_key(lines[index], key) {
            if !inserted {
                if let Some(replacement) = replacement {
                    updated.push(replacement);
                }
                inserted = true;
            }
            index = yaml_entry_end(&lines, index);
        } else {
            updated.push(lines[index]);
            index += 1;
        }
    }
    if !inserted {
        if let Some(replacement) = replacement {
            updated.insert(0, replacement);
        }
    }
    updated.join("\n")
}

/// Find the next top-level entry while consuming the indented/block/flow
/// continuation belonging to the field being replaced. Top-level comments
/// and blank separators are intentionally left alone.
fn yaml_entry_end(lines: &[&str], start: usize) -> usize {
    let mut index = start + 1;
    while index < lines.len() {
        let line = lines[index];
        if is_any_top_level_key(line) || line.starts_with('#') || line.trim().is_empty() {
            break;
        }
        // Indented block scalars and collections clearly belong to the
        // preceding field. An indentation-free `- item` or `]` is a legal
        // YAML continuation too; consume it so replacing a tags sequence
        // cannot leave a dangling value behind.
        index += 1;
    }
    index
}

fn is_safe_field_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn is_any_top_level_key(line: &str) -> bool {
    if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('#') {
        return false;
    }
    let Some((key, rest)) = line.split_once(':') else {
        return false;
    };
    is_safe_field_key(key) && rest.chars().next().is_none_or(char::is_whitespace)
}

fn is_top_level_key(line: &str, key: &str) -> bool {
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    let Some(rest) = line
        .strip_prefix(key)
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return false;
    };
    rest.chars().next().is_none_or(char::is_whitespace)
}

fn is_document_delimiter_line(line: &str) -> bool {
    // An indented `---` may be content in a block scalar. Only a marker at
    // column zero can terminate the Markdown frontmatter envelope.
    matches!(line.trim_end_matches('\r'), "---" | "...")
}

fn find_closing_fence(yaml_body: &str) -> Option<Range<usize>> {
    if yaml_close_fence_len(yaml_body).is_some() {
        return Some(0..3);
    }
    let mut offset = 0;
    while let Some(rel) = yaml_body[offset..].find('\n') {
        let start = offset + rel + 1;
        if yaml_close_fence_len(&yaml_body[start..]).is_some() {
            return Some(start..start + 3);
        }
        offset = start;
    }
    None
}

/// A line that is exactly `---` or YAML document-end `...` (newline or EOF).
fn yaml_close_fence_len(s: &str) -> Option<usize> {
    for fence in ["---", "..."] {
        if let Some(rest) = s.strip_prefix(fence) {
            if rest.is_empty() || rest.starts_with('\n') || rest.starts_with("\r\n") {
                return Some(fence.len());
            }
        }
    }
    None
}

fn extract_yaml_scalar_key(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in yaml.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix(&prefix) {
            return parse_yaml_scalar(value);
        }
    }
    None
}

fn parse_yaml_scalar(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        // The field serializer uses YAML escapes for newlines, quotes, and
        // control characters. Decode them for the next WYSIWYG edit so a
        // no-op revisit never turns `\n` into two literal characters.
        let parsed = YamlLoader::load_from_str(&format!("value: {trimmed}"))
            .ok()
            .and_then(|mut documents| documents.pop())
            .and_then(|document| match document {
                Yaml::Hash(values) => {
                    values
                        .get(&Yaml::String("value".to_string()))
                        .and_then(|value| match value {
                            Yaml::String(value) => Some(value.clone()),
                            _ => None,
                        })
                }
                _ => None,
            });
        return parsed.or_else(|| Some(trimmed[1..trimmed.len() - 1].to_string()));
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_frontmatter_title() {
        let source = "---\ntitle: My Doc\n---\n\n# Hello";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("My Doc"));
        assert!(info.end_byte < source.find("# Hello").unwrap());
    }

    #[test]
    fn absent_frontmatter_returns_none() {
        assert!(parse_frontmatter("# No frontmatter").is_none());
    }

    #[test]
    fn missing_closing_fence_is_invalid() {
        assert!(parse_frontmatter("---\ntitle: X\n").is_none());
        assert!(parse_frontmatter("---not a fence").is_none());
        assert!(parse_frontmatter("title: X\n---\n").is_none());
    }

    #[test]
    fn title_extraction_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("---\ntitle: My Doc\n---\n", Some("My Doc")),
            ("---\ntitle: \"Quoted Title\"\n---\n", Some("Quoted Title")),
            ("---\ntitle: 'Single'\n---\n", Some("Single")),
            ("---\ntitle: \"Line\\nTwo\"\n---\n", Some("Line\nTwo")),
            ("---\ntitle:   spaced  \n---\n", Some("spaced")),
            ("---\nauthor: me\n---\n", None),
            ("---\n---\n", None),
            ("\u{feff}---\ntitle: BOM\n---\n", Some("BOM")),
        ];
        for &(source, expected) in cases {
            let info = parse_frontmatter(source).unwrap();
            assert_eq!(info.title.as_deref(), expected, "source {source:?}");
        }
    }

    #[test]
    fn nested_dashes_in_body_do_not_extend_frontmatter() {
        let source = "---\ntitle: Doc\n---\n\n# Hello\n\n---\n\nStill body\n";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("Doc"));
        let body = &source[info.end_byte..];
        assert!(body.contains("# Hello"));
        assert!(body.contains("---"));
        assert!(body.contains("Still body"));
        assert!(!source[..info.end_byte].contains("Still body"));
    }

    #[test]
    fn nested_yaml_title_is_not_extracted_from_indented_key() {
        let source = "---\nmeta:\n  title: Nested\n---\n\n# Body\n";
        let info = parse_frontmatter(source).unwrap();
        assert!(
            info.title.is_none(),
            "nested title should not match: {info:?}"
        );
        assert!(info.end_byte < source.find("# Body").unwrap());
    }

    #[test]
    fn empty_title_value_is_none() {
        let info = parse_frontmatter("---\ntitle:\n---\n").unwrap();
        assert!(info.title.is_none());
    }

    #[test]
    fn crlf_frontmatter() {
        let source = "---\r\ntitle: Win\r\n---\r\n\r\n# Body";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("Win"));
        assert!(info.end_byte < source.find("# Body").unwrap());
    }

    #[test]
    fn extracts_top_level_tags() {
        let info = parse_frontmatter("---\ntitle: Doc\ntags: [a, b]\n---\n").unwrap();
        assert_eq!(info.tags.as_deref(), Some("[a, b]"));
        let nested = parse_frontmatter("---\nmeta:\n  tags: no\n---\n").unwrap();
        assert!(nested.tags.is_none());
    }

    #[test]
    fn extracts_description_and_yaml_body() {
        let source = "---\ntitle: Doc\ndescription: A note\ntags: [a]\nextra: 1\n---\n\n# Body\n";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.description.as_deref(), Some("A note"));
        assert!(info.yaml_body.contains("title: Doc"));
        assert!(info.yaml_body.contains("extra: 1"));
        assert!(!info.yaml_body.contains("# Body"));
        let nested = parse_frontmatter("---\nmeta:\n  description: no\n---\n").unwrap();
        assert!(nested.description.is_none());
    }

    #[test]
    fn upsert_yaml_key_inserts_replaces_and_removes() {
        let added = upsert_yaml_key("---\ntitle: Old\n---\n", "tags", "[x]").unwrap();
        assert!(added.contains("title: Old"), "{added}");
        assert!(added.contains("tags: [\"x\"]"), "{added}");
        let replaced = upsert_yaml_key(&added, "title", "New").unwrap();
        assert!(replaced.contains("title: \"New\""), "{replaced}");
        assert!(!replaced.contains("title: Old"), "{replaced}");
        let removed = upsert_yaml_key(&replaced, "tags", "").unwrap();
        assert!(!removed.contains("tags:"), "{removed}");
        assert!(removed.contains("title: \"New\""), "{removed}");
    }

    #[test]
    fn parses_yaml_ellipsis_closer() {
        let source = "---\ntitle: x\n...\nbody";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("x"));
        assert_eq!(&source[info.end_byte..], "body");
        assert!(!info.yaml_body.contains("body"));
        assert!(!info.yaml_body.contains("..."));
        assert_eq!(
            ellipsis_frontmatter_closer_range(source),
            Some(source.find("...").unwrap()..source.find("...").unwrap() + 3)
        );
    }

    #[test]
    fn ellipsis_closer_at_eof_without_newline() {
        let source = "---\ntitle: x\n...";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("x"));
        assert_eq!(info.end_byte, source.len());
        assert_eq!(comrak_parse_input(source).as_ref(), "---\ntitle: x\n---");
    }

    #[test]
    fn empty_ellipsis_frontmatter() {
        let source = "---\n...\nbody";
        let info = parse_frontmatter(source).unwrap();
        assert!(info.yaml_body.is_empty());
        assert_eq!(&source[info.end_byte..], "body");
    }

    #[test]
    fn crlf_ellipsis_frontmatter() {
        let source = "---\r\ntitle: Win\r\n...\r\n\r\n# Body";
        let info = parse_frontmatter(source).unwrap();
        assert_eq!(info.title.as_deref(), Some("Win"));
        assert!(info.end_byte < source.find("# Body").unwrap());
    }

    #[test]
    fn mid_document_ellipsis_is_not_frontmatter() {
        assert!(parse_frontmatter("# Hello\n...\nbody").is_none());
        assert!(parse_frontmatter("hello\n...\nworld").is_none());
        assert!(parse_frontmatter("hello\n\n---\ntitle: x\n...\n").is_none());
        let dashed = "---\ntitle: Doc\n---\n\n# Hello\n\n...\n\nStill body\n";
        let info = parse_frontmatter(dashed).unwrap();
        let body = &dashed[info.end_byte..];
        assert!(body.contains("# Hello") && body.contains("...") && body.contains("Still body"));
        assert!(!dashed[..info.end_byte].contains("Still body"));
    }

    #[test]
    fn upsert_preserves_ellipsis_closer() {
        let updated = upsert_yaml_key("---\ntitle: Old\n...\n", "title", "New").unwrap();
        assert!(updated.contains("title: \"New\""), "{updated}");
        assert!(updated.starts_with("---\n"), "{updated}");
        assert!(
            updated.contains("\n...\n") && !updated.contains("\n---\n"),
            "ellipsis closer must stay, {updated}"
        );
    }

    #[test]
    fn field_values_are_serialized_not_concatenated_as_yaml() {
        let updated = upsert_yaml_key(
            "---\ntitle: Old\nowner: Alex\n---\n",
            "title",
            "A: \"quoted\" # text\nowner: Mallory",
        )
        .unwrap();
        assert!(
            updated.contains(r#"title: "A: \"quoted\" # text\nowner: Mallory""#),
            "{updated}"
        );
        assert!(updated.contains("owner: Alex"), "{updated}");
        assert!(!updated.contains("\nowner: Mallory\n"), "{updated}");
        let info = parse_frontmatter(&updated).unwrap();
        validate_frontmatter_yaml(&info.yaml_body).unwrap();
    }

    #[test]
    fn tags_are_a_safe_scalar_sequence() {
        let updated = upsert_yaml_key(
            "---\ntitle: Doc\n---\n",
            "tags",
            "docs, team: core\nowner: Mallory",
        )
        .unwrap();
        assert!(
            updated.contains(r#"tags: ["docs", "team: core\nowner: Mallory"]"#),
            "{updated}"
        );
        let info = parse_frontmatter(&updated).unwrap();
        validate_frontmatter_yaml(&info.yaml_body).unwrap();
    }

    #[test]
    fn field_update_replaces_a_multiline_value_without_touching_other_metadata() {
        let updated = upsert_yaml_key(
            "---\ntitle: |\n  Old title\n  continues\n# retained\nmeta:\n  owner: Alex\n---\n",
            "title",
            "New title",
        )
        .unwrap();
        assert!(updated.contains("title: \"New title\""), "{updated}");
        assert!(!updated.contains("Old title"), "{updated}");
        assert!(updated.contains("# retained"), "{updated}");
        assert!(updated.contains("meta:\n  owner: Alex"), "{updated}");
        let info = parse_frontmatter(&updated).unwrap();
        validate_frontmatter_yaml(&info.yaml_body).unwrap();
    }

    #[test]
    fn invalid_yaml_and_delimiters_are_rejected_before_writing() {
        assert_eq!(
            validate_frontmatter_yaml("title: [unterminated"),
            Err(FrontmatterError::InvalidYaml)
        );
        assert_eq!(
            normalize_frontmatter("title: Safe\n---\n# body"),
            Err(FrontmatterError::UnexpectedDelimiter)
        );
        assert_eq!(
            normalize_frontmatter("title: [unterminated"),
            Err(FrontmatterError::InvalidYaml)
        );
        assert_eq!(
            upsert_yaml_key("---\ntitle: [unterminated\n---\n", "title", "Safe"),
            Err(FrontmatterError::InvalidYaml)
        );
    }

    #[test]
    fn indented_delimiter_text_is_valid_block_scalar_content() {
        let body = "description: |\n  first line\n  ---\n  ...";
        validate_frontmatter_yaml(body).unwrap();
        let normalized = normalize_frontmatter(body).unwrap();
        assert!(normalized.contains("  ---\n  ..."), "{normalized}");
    }
}
