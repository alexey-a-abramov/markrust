// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Parsed YAML frontmatter metadata for UI hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub title: Option<String>,
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
        title: extract_yaml_title(yaml),
    })
}

fn find_closing_fence(yaml_body: &str) -> Option<std::ops::Range<usize>> {
    for (idx, _) in yaml_body.match_indices("\n---") {
        let rest = &yaml_body[idx + 1..];
        if rest.starts_with("---\n") || rest.starts_with("---\r\n") || rest == "---" {
            return Some(idx + 1..idx + 4);
        }
    }
    None
}

fn extract_yaml_title(yaml: &str) -> Option<String> {
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("title:") {
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
}
