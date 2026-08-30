// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Parsed YAML frontmatter metadata for UI hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub title: Option<String>,
    pub tags: Option<String>,
}

/// Detect `---` YAML frontmatter at the document start and extract a display title.
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
        tags: extract_yaml_scalar_key(yaml, "tags"),
    })
}

/// Insert or replace a top-level YAML key in a frontmatter blob (with or
/// without `---` fences). Empty `value` removes the key.
pub fn upsert_yaml_key(raw: &str, key: &str, value: &str) -> String {
    let trimmed = raw.trim();
    let (had_fences, body) = strip_frontmatter_fences(trimmed);
    let mut lines: Vec<String> = if body.is_empty() {
        Vec::new()
    } else {
        body.lines().map(str::to_string).collect()
    };
    if value.is_empty() {
        lines.retain(|line| !is_top_level_key(line, key));
    } else {
        let rendered = format!("{key}: {value}");
        if let Some(existing) = lines.iter_mut().find(|line| is_top_level_key(line, key)) {
            *existing = rendered;
        } else {
            lines.insert(0, rendered);
        }
    }
    let mut yaml = lines.join("\n");
    if !yaml.is_empty() && !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    if had_fences || trimmed.starts_with("---") || trimmed.is_empty() {
        if yaml.is_empty() {
            String::new()
        } else {
            format!("---\n{yaml}---\n")
        }
    } else {
        yaml
    }
}

fn strip_frontmatter_fences(raw: &str) -> (bool, String) {
    let trimmed = raw.trim();
    if !trimmed.starts_with("---") {
        return (false, trimmed.to_string());
    }
    let rest = trimmed.trim_start_matches("---").trim_start_matches('\r');
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    if let Some(idx) = rest.find("\n---") {
        (true, rest[..idx].trim_matches('\n').to_string())
    } else if rest == "---" || rest.starts_with("---") {
        (true, String::new())
    } else {
        (true, rest.trim_end_matches("---").trim().to_string())
    }
}

fn is_top_level_key(line: &str, key: &str) -> bool {
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    let trimmed = line.trim_end();
    trimmed == format!("{key}:") || trimmed.starts_with(&format!("{key}:"))
}

fn find_closing_fence(yaml_body: &str) -> Option<std::ops::Range<usize>> {
    if yaml_body.starts_with("---\n") || yaml_body.starts_with("---\r\n") || yaml_body == "---" {
        return Some(0..3);
    }
    for (idx, _) in yaml_body.match_indices("\n---") {
        let rest = &yaml_body[idx + 1..];
        if rest.starts_with("---\n") || rest.starts_with("---\r\n") || rest == "---" {
            return Some(idx + 1..idx + 4);
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
        return Some(trimmed[1..trimmed.len() - 1].to_string());
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
    fn upsert_yaml_key_inserts_replaces_and_removes() {
        let added = upsert_yaml_key("---\ntitle: Old\n---\n", "tags", "[x]");
        assert!(added.contains("title: Old"), "{added}");
        assert!(added.contains("tags: [x]"), "{added}");
        let replaced = upsert_yaml_key(&added, "title", "New");
        assert!(replaced.contains("title: New"), "{replaced}");
        assert!(!replaced.contains("title: Old"), "{replaced}");
        let removed = upsert_yaml_key(&replaced, "tags", "");
        assert!(!removed.contains("tags:"), "{removed}");
        assert!(removed.contains("title: New"), "{removed}");
    }
}
