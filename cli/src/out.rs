//! Rows written as aligned columns, CSV or JSON lines.

use std::io::{self, Write};

use crate::args::Format;

/// A cell: text, a number written bare in JSON, or nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    /// Text.
    Text(String),
    /// A number or boolean, already formatted; bare in JSON.
    Num(String),
    /// No value.
    Null,
}

impl Cell {
    fn text(&self) -> &str {
        match self {
            Cell::Text(s) | Cell::Num(s) => s,
            Cell::Null => "",
        }
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::Text(s)
    }
}

impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::Text(s.to_owned())
    }
}

impl<T: Into<Cell>> From<Option<T>> for Cell {
    fn from(v: Option<T>) -> Self {
        v.map_or(Cell::Null, Into::into)
    }
}

/// A number cell.
pub fn num(n: impl std::fmt::Display) -> Cell {
    Cell::Num(n.to_string())
}

/// Columns and rows.
#[derive(Debug)]
pub struct Table {
    head: &'static [&'static str],
    rows: Vec<Vec<Cell>>,
}

impl Table {
    /// An empty table with these columns.
    pub fn new(head: &'static [&'static str]) -> Self {
        Table {
            head,
            rows: Vec::new(),
        }
    }

    /// Adds a row of one cell a column.
    pub fn row(&mut self, cells: Vec<Cell>) {
        debug_assert_eq!(cells.len(), self.head.len());
        self.rows.push(cells);
    }

    /// Writes the table; `head` says whether the header line of a table or CSV is written.
    pub fn write(&self, format: Format, head: bool, out: &mut dyn Write) -> io::Result<()> {
        match format {
            Format::Table => {
                let mut widths: Vec<usize> = self.head.iter().map(|h| h.chars().count()).collect();
                for r in &self.rows {
                    for (w, c) in widths.iter_mut().zip(r) {
                        *w = (*w).max(c.text().chars().count());
                    }
                }
                let line = |out: &mut dyn Write, cells: &mut dyn Iterator<Item = &str>| {
                    let mut s = String::new();
                    for (k, c) in cells.enumerate() {
                        if k > 0 {
                            s.push_str("  ");
                        }
                        s.push_str(c);
                        s.extend(std::iter::repeat_n(
                            ' ',
                            widths[k].saturating_sub(c.chars().count()),
                        ));
                    }
                    writeln!(out, "{}", s.trim_end())
                };
                if head {
                    line(out, &mut self.head.iter().copied())?;
                }
                for r in &self.rows {
                    line(out, &mut r.iter().map(Cell::text))?;
                }
            }
            Format::Csv => {
                if head {
                    writeln!(out, "{}", self.head.join(","))?;
                }
                for r in &self.rows {
                    let cells: Vec<String> = r.iter().map(|c| csv(c.text())).collect();
                    writeln!(out, "{}", cells.join(","))?;
                }
            }
            Format::Jsonl => {
                for r in &self.rows {
                    let mut s = String::from("{");
                    for (k, (h, c)) in self.head.iter().zip(r).enumerate() {
                        if k > 0 {
                            s.push(',');
                        }
                        s.push_str(&json(h));
                        s.push(':');
                        match c {
                            Cell::Text(t) => s.push_str(&json(t)),
                            Cell::Num(n) => s.push_str(n),
                            Cell::Null => s.push_str("null"),
                        }
                    }
                    s.push('}');
                    writeln!(out, "{s}")?;
                }
            }
        }
        Ok(())
    }
}

fn csv(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(format: Format) -> String {
        let mut t = Table::new(&["name", "value"]);
        t.row(vec!["a,\"b\"".into(), num(1.5)]);
        t.row(vec!["tab\there".into(), Cell::Null]);
        let mut out = Vec::new();
        t.write(format, true, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn each_format_quotes_what_it_must() {
        assert_eq!(
            written(Format::Table),
            "name      value\na,\"b\"     1.5\ntab\there\n"
        );
        assert_eq!(
            written(Format::Csv),
            "name,value\n\"a,\"\"b\"\"\",1.5\ntab\there,\n"
        );
        assert_eq!(
            written(Format::Jsonl),
            "{\"name\":\"a,\\\"b\\\"\",\"value\":1.5}\n{\"name\":\"tab\\there\",\"value\":null}\n"
        );
    }

    #[test]
    fn json_escapes_every_control_character() {
        let mut t = Table::new(&["v"]);
        t.row(vec!["a\u{1}b\u{1f}".into()]);
        let mut out = Vec::new();
        t.write(Format::Jsonl, true, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"v\":\"a\\u0001b\\u001f\"}\n"
        );
    }
}
