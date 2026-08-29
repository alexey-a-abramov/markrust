// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure GFM pipe-table alignment helpers (no GPUI).

/// Horizontal alignment for a GFM table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
}

/// Parse a delimiter row such as `| :--- | :---: | ---: |`.
pub fn parse_column_alignments(line: &str) -> Vec<ColumnAlign> {
    split_table_cells(line)
        .into_iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            match (left, right) {
                (true, true) => ColumnAlign::Center,
                (false, true) => ColumnAlign::Right,
                _ => ColumnAlign::Left,
            }
        })
        .collect()
}

pub fn split_table_cells(line: &str) -> Vec<String> {
    line.split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn compute_column_widths(lines: &[&str]) -> Vec<usize> {
    let mut widths = Vec::new();
    for line in lines {
        for (idx, cell) in split_table_cells(line).into_iter().enumerate() {
            if idx >= widths.len() {
                widths.push(cell.chars().count());
            } else {
                widths[idx] = widths[idx].max(cell.chars().count());
            }
        }
    }
    widths.into_iter().map(|w| w.max(1)).collect()
}

pub fn format_data_row(line: &str, widths: &[usize], alignments: &[ColumnAlign]) -> String {
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return line.to_string();
    }
    let mut parts = vec!["|".to_string()];
    for (idx, cell) in cells.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(cell.chars().count());
        let align = alignments.get(idx).copied().unwrap_or(ColumnAlign::Left);
        parts.push(format!(" {} ", pad_cell(cell, width, align)));
        parts.push("|".to_string());
    }
    parts.join("")
}

pub fn format_delimiter_row(line: &str, widths: &[usize], alignments: &[ColumnAlign]) -> String {
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return line.to_string();
    }
    let mut parts = vec!["|".to_string()];
    for (idx, _cell) in cells.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(3).max(3);
        let align = alignments.get(idx).copied().unwrap_or(ColumnAlign::Left);
        let dashes = "-".repeat(width);
        let body = match align {
            ColumnAlign::Left => format!(":{dashes}"),
            ColumnAlign::Center => format!(":{dashes}:"),
            ColumnAlign::Right => format!("{dashes}:"),
        };
        parts.push(format!(" {body} "));
        parts.push("|".to_string());
    }
    parts.join("")
}

pub fn pad_cell(cell: &str, width: usize, align: ColumnAlign) -> String {
    let len = cell.chars().count();
    if len >= width {
        return cell.to_string();
    }
    let pad = width - len;
    match align {
        ColumnAlign::Left => format!("{cell}{}", " ".repeat(pad)),
        ColumnAlign::Right => format!("{}{cell}", " ".repeat(pad)),
        ColumnAlign::Center => {
            let left = pad / 2;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_left_center_right_and_default_left() {
        assert_eq!(
            parse_column_alignments("| :--- | :---: | ---: | --- |"),
            vec![
                ColumnAlign::Left,
                ColumnAlign::Center,
                ColumnAlign::Right,
                ColumnAlign::Left,
            ]
        );
    }

    #[test]
    fn empty_or_plain_line_has_no_columns() {
        assert!(parse_column_alignments("").is_empty());
        assert_eq!(
            parse_column_alignments("not a table"),
            vec![ColumnAlign::Left]
        );
        assert_eq!(split_table_cells("|  |  |"), Vec::<String>::new());
    }

    #[test]
    fn split_ignores_outer_pipes_and_empty_cells() {
        assert_eq!(split_table_cells("| a | bb |"), vec!["a", "bb"]);
        assert_eq!(split_table_cells("a|b"), vec!["a", "b"]);
    }

    #[test]
    fn column_widths_use_widest_cell() {
        let lines = ["| a | bb |", "| ccc | d |"];
        assert_eq!(compute_column_widths(&lines), vec![3, 2]);
    }

    #[test]
    fn pads_left_center_right() {
        assert_eq!(pad_cell("a", 3, ColumnAlign::Left), "a  ");
        assert_eq!(pad_cell("a", 3, ColumnAlign::Right), "  a");
        assert_eq!(pad_cell("a", 4, ColumnAlign::Center).chars().count(), 4);
        assert_eq!(pad_cell("abcd", 2, ColumnAlign::Left), "abcd");
    }

    #[test]
    fn formats_aligned_data_and_delimiter_rows() {
        let widths = vec![3, 3];
        let alignments = vec![ColumnAlign::Left, ColumnAlign::Right];
        let data = format_data_row("| a | b |", &widths, &alignments);
        assert!(data.starts_with('|') && data.ends_with('|'));
        assert!(data.contains(" a   "));
        let delim = format_delimiter_row("|---|---|", &widths, &alignments);
        assert!(delim.contains(":---"));
        assert!(delim.contains("---:"));
        assert_eq!(format_data_row("", &widths, &alignments), "");
        assert_eq!(format_delimiter_row("", &widths, &alignments), "");
    }

    #[test]
    fn cjk_cells_count_characters_not_bytes() {
        let lines = ["| 你好 | x |"];
        assert_eq!(compute_column_widths(&lines), vec![2, 1]);
    }
}
