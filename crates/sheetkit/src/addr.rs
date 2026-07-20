//! A1-style cell and range addressing.
//!
//! Targets accepted throughout the toolkit:
//! - `A1` — a single cell
//! - `A1:C10` — a range
//! - `Sheet2!B3`, `'My Sheet'!A1:C10` — sheet-qualified
//! - `C` or `C:E` — whole columns (resolved against the used range)
//! - `5:20` — whole rows
//! - a bare identifier — resolved later as a region, defined name, or sheet name

use std::fmt;

pub use ironcalc_base::expressions::utils::{column_to_number, number_to_column};

/// A cell position within a sheet (1-indexed, like the engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellRef {
    pub row: i32,
    pub col: i32,
}

impl CellRef {
    pub fn a1(&self) -> String {
        format!(
            "{}{}",
            number_to_column(self.col).unwrap_or_default(),
            self.row
        )
    }
}

/// A rectangular range within a sheet, inclusive on both ends. Always normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: CellRef,
    pub end: CellRef,
}

impl Range {
    pub fn cell(c: CellRef) -> Range {
        Range { start: c, end: c }
    }

    pub fn new(a: CellRef, b: CellRef) -> Range {
        Range {
            start: CellRef {
                row: a.row.min(b.row),
                col: a.col.min(b.col),
            },
            end: CellRef {
                row: a.row.max(b.row),
                col: a.col.max(b.col),
            },
        }
    }

    pub fn is_single_cell(&self) -> bool {
        self.start == self.end
    }

    pub fn width(&self) -> i32 {
        self.end.col - self.start.col + 1
    }

    pub fn height(&self) -> i32 {
        self.end.row - self.start.row + 1
    }

    pub fn cell_count(&self) -> i64 {
        self.width() as i64 * self.height() as i64
    }

    pub fn contains(&self, row: i32, col: i32) -> bool {
        row >= self.start.row && row <= self.end.row && col >= self.start.col && col <= self.end.col
    }

    pub fn a1(&self) -> String {
        if self.is_single_cell() {
            self.start.a1()
        } else {
            format!("{}:{}", self.start.a1(), self.end.a1())
        }
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.a1())
    }
}

/// What a target string refers to, before sheet-name/region/defined-name resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Range(Range),
    /// Whole columns, e.g. `C` or `C:E`.
    Cols {
        start: i32,
        end: i32,
    },
    /// Whole rows, e.g. `5:20`.
    Rows {
        start: i32,
        end: i32,
    },
    /// A bare identifier: region name, defined name, or sheet name.
    Ident(String),
}

/// A parsed target: optional sheet qualifier plus what it addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub sheet: Option<String>,
    pub kind: TargetKind,
}

/// Parse one endpoint of a range: `A1`, `C` (column), or `5` (row).
#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    Cell(CellRef),
    Col(i32),
    Row(i32),
}

fn parse_endpoint(s: &str) -> Option<Endpoint> {
    // `$` markers are accepted and ignored; absolute and relative addresses
    // resolve to the same place.
    let s = s.trim().strip_prefix('$').unwrap_or_else(|| s.trim());
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let rest = &s[letters.len()..];
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    if letters.is_empty() {
        let row: i32 = rest.parse().ok()?;
        if !(1..=1_048_576).contains(&row) {
            return None;
        }
        return Some(Endpoint::Row(row));
    }
    if letters.len() > 3 {
        return None;
    }
    let col = column_to_number(&letters.to_ascii_uppercase()).ok()?;
    if rest.is_empty() {
        return Some(Endpoint::Col(col));
    }
    let row: i32 = rest.parse().ok()?;
    if !(1..=1_048_576).contains(&row) {
        return None;
    }
    Some(Endpoint::Cell(CellRef { row, col }))
}

/// Split `Sheet!rest` into (sheet, rest), honoring `'quoted sheet names'`.
fn split_sheet(s: &str) -> (Option<String>, &str) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('\'') {
        if let Some(q) = rest.find('\'') {
            let name = &rest[..q];
            let after = &rest[q + 1..];
            if let Some(after) = after.strip_prefix('!') {
                return (Some(name.to_string()), after);
            }
        }
        return (None, s);
    }
    match s.find('!') {
        Some(i) => (Some(s[..i].to_string()), &s[i + 1..]),
        None => (None, s),
    }
}

/// Parse a target string. Returns `None` when it is not a syntactically valid target.
pub fn parse_target(input: &str) -> Option<Target> {
    let (sheet, rest) = split_sheet(input);
    let rest = rest.trim();
    if rest.is_empty() {
        // `Sheet1!` is not valid, but a bare quoted sheet could land here.
        return None;
    }

    // Bare identifier (region / defined name / sheet name)? Anything that is not
    // parseable as a reference and looks like an identifier.
    let as_ref = parse_range_str(rest);
    if let Some(kind) = as_ref {
        return Some(Target { sheet, kind });
    }
    if sheet.is_none() && is_ident(rest) {
        return Some(Target {
            sheet: None,
            kind: TargetKind::Ident(rest.to_string()),
        });
    }
    // `Sheet1` alone parses as a sheet target via Ident; `Sheet1!foo` is invalid.
    None
}

fn parse_range_str(s: &str) -> Option<TargetKind> {
    match s.split_once(':') {
        None => match parse_endpoint(s)? {
            Endpoint::Cell(c) => Some(TargetKind::Range(Range::cell(c))),
            Endpoint::Col(c) => Some(TargetKind::Cols { start: c, end: c }),
            Endpoint::Row(_) => None, // a bare number is not a target
        },
        Some((a, b)) => {
            let (a, b) = (parse_endpoint(a)?, parse_endpoint(b)?);
            match (a, b) {
                (Endpoint::Cell(a), Endpoint::Cell(b)) => Some(TargetKind::Range(Range::new(a, b))),
                (Endpoint::Col(a), Endpoint::Col(b)) => Some(TargetKind::Cols {
                    start: a.min(b),
                    end: a.max(b),
                }),
                (Endpoint::Row(a), Endpoint::Row(b)) => Some(TargetKind::Rows {
                    start: a.min(b),
                    end: a.max(b),
                }),
                _ => None,
            }
        }
    }
}

pub fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == ' ')
}

/// Quote a sheet name for display when it needs it.
pub fn display_sheet(name: &str) -> String {
    if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        name.to_string()
    } else {
        format!("'{name}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: i32, col: i32) -> CellRef {
        CellRef { row, col }
    }

    #[test]
    fn single_cell() {
        let t = parse_target("B3").unwrap();
        assert_eq!(t.sheet, None);
        assert_eq!(t.kind, TargetKind::Range(Range::cell(cell(3, 2))));
    }

    #[test]
    fn range_normalizes() {
        let t = parse_target("C10:A1").unwrap();
        assert_eq!(
            t.kind,
            TargetKind::Range(Range::new(cell(1, 1), cell(10, 3)))
        );
    }

    #[test]
    fn sheet_qualified() {
        let t = parse_target("Sheet2!A1:B2").unwrap();
        assert_eq!(t.sheet.as_deref(), Some("Sheet2"));
        let t = parse_target("'My Sheet'!D4").unwrap();
        assert_eq!(t.sheet.as_deref(), Some("My Sheet"));
    }

    #[test]
    fn columns_and_rows() {
        assert_eq!(
            parse_target("C").unwrap().kind,
            TargetKind::Cols { start: 3, end: 3 }
        );
        assert_eq!(
            parse_target("C:E").unwrap().kind,
            TargetKind::Cols { start: 3, end: 5 }
        );
        assert_eq!(
            parse_target("5:20").unwrap().kind,
            TargetKind::Rows { start: 5, end: 20 }
        );
    }

    #[test]
    fn idents() {
        assert_eq!(
            parse_target("orders").unwrap().kind,
            TargetKind::Ident("orders".into())
        );
        // `A1` wins over ident interpretation
        assert!(matches!(
            parse_target("A1").unwrap().kind,
            TargetKind::Range(_)
        ));
    }

    #[test]
    fn absolute_refs_accepted() {
        let t = parse_target("$B$3").unwrap();
        assert_eq!(t.kind, TargetKind::Range(Range::cell(cell(3, 2))));
    }

    #[test]
    fn a1_roundtrip() {
        assert_eq!(cell(3, 28).a1(), "AB3");
        assert_eq!(Range::new(cell(1, 1), cell(10, 3)).a1(), "A1:C10");
    }
}
