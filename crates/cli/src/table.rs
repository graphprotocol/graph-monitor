//! A minimal table: header row, one rule, space-aligned columns.
//!
//! No outer borders — the goal is something that reads cleanly in a terminal and survives
//! being pasted into a chat.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Row>,
}

enum Row {
    Cells(Vec<String>),
    /// A full-width label spanning the table, used for group headings.
    Section(String),
}

impl Table {
    pub fn new(columns: &[(&str, Align)]) -> Self {
        Self {
            headers: columns.iter().map(|(h, _)| h.to_string()).collect(),
            aligns: columns.iter().map(|(_, a)| *a).collect(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Vec<String>) {
        debug_assert_eq!(row.len(), self.headers.len());
        self.rows.push(Row::Cells(row));
    }

    pub fn section(&mut self, title: impl Into<String>) {
        self.rows.push(Row::Section(title.into()));
    }

    pub fn is_empty(&self) -> bool {
        !self.rows.iter().any(|r| matches!(r, Row::Cells(_)))
    }

    /// Columns where every cell is empty, so they can be left out entirely.
    ///
    /// A column of nothing still costs its header width and reads as missing data rather than
    /// as "nobody asked for this".
    fn occupied(&self) -> Vec<bool> {
        let mut used = vec![false; self.headers.len()];
        for row in &self.rows {
            if let Row::Cells(cells) = row {
                for (i, cell) in cells.iter().enumerate() {
                    if !cell.trim().is_empty() {
                        used[i] = true;
                    }
                }
            }
        }
        used
    }

    pub fn render(&self) -> String {
        const GAP: usize = 3;

        let keep = self.occupied();
        let headers: Vec<&String> = self
            .headers
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(h, _)| h)
            .collect();
        if headers.is_empty() {
            return String::new();
        }
        let aligns: Vec<Align> = self
            .aligns
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(a, _)| *a)
            .collect();
        let rows: Vec<Row> = self
            .rows
            .iter()
            .map(|row| match row {
                Row::Section(t) => Row::Section(t.clone()),
                Row::Cells(cells) => Row::Cells(
                    cells
                        .iter()
                        .zip(&keep)
                        .filter(|(_, k)| **k)
                        .map(|(c, _)| c.clone())
                        .collect(),
                ),
            })
            .collect();

        let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
        for row in &rows {
            if let Row::Cells(cells) = row {
                for (i, cell) in cells.iter().enumerate() {
                    widths[i] = widths[i].max(display_width(cell));
                }
            }
        }

        let mut out = String::new();

        let header: Vec<String> = headers
            .iter()
            .enumerate()
            .map(|(i, h)| pad(h, widths[i], aligns[i]))
            .collect();
        let header_line = header.join(&" ".repeat(GAP));
        let rule_width = display_width(header_line.trim_end());

        // Column headers recede: they are a legend, read once. The data is the point.
        out.push_str(&crate::style::dim(header_line.trim_end()));
        out.push('\n');
        out.push_str(&crate::style::dim(&"─".repeat(rule_width.max(1))));
        out.push('\n');

        for (i, row) in rows.iter().enumerate() {
            match row {
                Row::Section(title) => {
                    // Breathing room between groups, but not directly under the rule.
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(title);
                    out.push('\n');
                }
                Row::Cells(cells) => {
                    let rendered: Vec<String> = cells
                        .iter()
                        .enumerate()
                        .map(|(i, c)| pad(c, widths[i], aligns[i]))
                        .collect();
                    out.push_str(rendered.join(&" ".repeat(GAP)).trim_end());
                    out.push('\n');
                }
            }
        }

        out
    }
}

/// Visible width: character count, skipping ANSI escape sequences.
///
/// Byte length would skew every column containing `─` or `…`, and a naive character count
/// would skew every styled cell by the length of its escape codes — which is how coloured
/// tables end up with ragged columns.
fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume through the terminating letter of the sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

fn pad(s: &str, width: usize, align: Align) -> String {
    let len = display_width(s);
    let fill = width.saturating_sub(len);
    match align {
        Align::Left => format!("{s}{}", " ".repeat(fill)),
        Align::Right => format!("{}{s}", " ".repeat(fill)),
    }
}

/// Thousands separators for a plain integer.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_matches_header_width_despite_multibyte_cells() {
        let mut t = Table::new(&[("Label", Align::Left), ("Value", Align::Right)]);
        t.push(vec!["0x1234…5678".into(), "1.00".into()]);
        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].chars().all(|c| c == '─'));
        assert_eq!(lines[1].chars().count(), lines[0].chars().count());
    }

    #[test]
    fn sections_span_the_table() {
        let mut t = Table::new(&[("A", Align::Left), ("B", Align::Left)]);
        t.section("group: billing");
        t.push(vec!["x".into(), "y".into()]);
        let out = t.render();
        assert!(out.contains("group: billing"));
    }

    #[test]
    fn a_table_with_only_sections_is_empty() {
        let mut t = Table::new(&[("A", Align::Left)]);
        assert!(t.is_empty());
        t.section("nothing here");
        assert!(t.is_empty());
        t.push(vec!["x".into()]);
        assert!(!t.is_empty());
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group_thousands(505_090_074), "505,090,074");
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
    }
}
