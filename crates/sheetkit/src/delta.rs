//! The delta engine: snapshot computed values before a command batch, diff
//! after, and report exactly what changed — the "recalc echo" that lets a
//! model trust the workbook state without re-reading it.

use std::collections::HashMap;

use crate::addr::{display_sheet, CellRef};
use crate::book::{cell_to_value, Book, Value};

/// Computed values of every non-empty cell, keyed by `(sheet, row, col)`.
pub struct Snapshot {
    cells: HashMap<(u32, i32, i32), Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellChange {
    pub sheet: u32,
    pub sheet_name: String,
    pub row: i32,
    pub col: i32,
    pub old: Value,
    pub new: Value,
}

impl CellChange {
    pub fn addr(&self) -> String {
        CellRef { row: self.row, col: self.col }.a1()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delta {
    pub changes: Vec<CellChange>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Count of changes that produced an error value.
    pub fn new_errors(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| matches!(c.new, Value::Error(_)) && !matches!(c.old, Value::Error(_)))
            .count()
    }
}

pub fn snapshot(book: &Book) -> Snapshot {
    let mut cells = HashMap::new();
    let shared = book.shared_strings();
    for sheet in 0..book.sheet_count() {
        book.for_each_cell(sheet, |row, col, cell| {
            let v = cell_to_value(cell, shared);
            if !v.is_empty() {
                cells.insert((sheet, row, col), v);
            }
        });
    }
    Snapshot { cells }
}

/// Diff a snapshot against the current state of the book.
pub fn diff(before: &Snapshot, book: &Book) -> Delta {
    let mut changes = Vec::new();
    let sheet_names = book.sheet_names();
    let mut seen: HashMap<(u32, i32, i32), ()> = HashMap::new();

    let shared = book.shared_strings();
    for sheet in 0..book.sheet_count() {
        let sheet_name = sheet_names
            .get(sheet as usize)
            .cloned()
            .unwrap_or_default();
        book.for_each_cell(sheet, |row, col, cell| {
            let new = cell_to_value(cell, shared);
            if new.is_empty() {
                return;
            }
            seen.insert((sheet, row, col), ());
            let old = before
                .cells
                .get(&(sheet, row, col))
                .cloned()
                .unwrap_or(Value::Empty);
            if old != new {
                changes.push(CellChange {
                    sheet,
                    sheet_name: sheet_name.clone(),
                    row,
                    col,
                    old,
                    new,
                });
            }
        });
    }

    // Cells that existed before and are now empty (cleared / deleted).
    for (&(sheet, row, col), old) in &before.cells {
        if !seen.contains_key(&(sheet, row, col)) {
            changes.push(CellChange {
                sheet,
                sheet_name: sheet_names.get(sheet as usize).cloned().unwrap_or_default(),
                row,
                col,
                old: old.clone(),
                new: Value::Empty,
            });
        }
    }

    changes.sort_by_key(|c| (c.sheet, c.col, c.row));
    Delta { changes }
}

/// Render the delta echo, bounded to `max_lines` explicit changes.
///
/// ```text
/// recalc: 3 cells changed
///   D2 7 ⇒ 14 · D5 23 ⇒ 30 · F9 #DIV/0! ⇒ 12  (2 errors fixed)
/// ```
pub fn render(delta: &Delta, multi_sheet: bool, max_lines: usize) -> String {
    if delta.is_empty() {
        return "recalc: no cell values changed".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for c in delta.changes.iter().take(max_lines) {
        let addr = if multi_sheet {
            format!("{}!{}", display_sheet(&c.sheet_name), c.addr())
        } else {
            c.addr()
        };
        parts.push(format!("{addr} {} ⇒ {}", c.old.display(), c.new.display()));
    }
    let mut out = format!(
        "recalc: {} cell{} changed\n  {}",
        delta.len(),
        if delta.len() == 1 { "" } else { "s" },
        parts.join(" · ")
    );
    if delta.len() > max_lines {
        out.push_str(&format!("\n  … {} more (use `view` to inspect)", delta.len() - max_lines));
    }
    let errors = delta.new_errors();
    if errors > 0 {
        out.push_str(&format!("\n  ⚠ {errors} new error{}", if errors == 1 { "" } else { "s" }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ripple() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "2").unwrap(); // A1
        book.set_input(0, 1, 2, "=A1*10").unwrap(); // B1
        book.set_input(0, 1, 3, "=B1+1").unwrap(); // C1

        let before = snapshot(&book);
        book.set_input(0, 1, 1, "5").unwrap();
        let d = diff(&before, &book);

        assert_eq!(d.len(), 3); // A1, B1, C1 all changed
        let b1 = d.changes.iter().find(|c| c.col == 2).unwrap();
        assert_eq!(b1.old, Value::Number(20.0));
        assert_eq!(b1.new, Value::Number(50.0));
    }

    #[test]
    fn detects_cleared_cells() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "9").unwrap();
        let before = snapshot(&book);
        book.clear_contents(0, crate::addr::Range::cell(CellRef { row: 1, col: 1 }))
            .unwrap();
        let d = diff(&before, &book);
        assert_eq!(d.len(), 1);
        assert_eq!(d.changes[0].new, Value::Empty);
    }

    #[test]
    fn render_reports_errors() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "1").unwrap();
        book.set_input(0, 2, 1, "=A1/1").unwrap();
        let before = snapshot(&book);
        book.set_input(0, 1, 1, "0").unwrap();
        book.set_input(0, 2, 1, "=1/A1").unwrap();
        let d = diff(&before, &book);
        let text = render(&d, false, 10);
        assert!(text.contains("#DIV/0!"), "{text}");
        assert!(text.contains("new error"), "{text}");
    }
}
