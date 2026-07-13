//! Fill-run detection.
//!
//! IronCalc stores formulas deduplicated in canonical R1C1 per sheet; every
//! formula cell holds an index into that list. Two cells in a column with the
//! same index are the *same relative formula* — a fill. That makes uniform
//! ranges (`D2:D9871 all =B{r}*C{r}`) detectable by grouping indices, and
//! cells that break the pattern stand out as deviations worth flagging.

use crate::addr::{CellRef, Range};
use crate::book::Book;

/// A vertical run of cells sharing one relative formula.
#[derive(Debug, Clone, PartialEq)]
pub struct FillRun {
    pub sheet: u32,
    pub col: i32,
    pub row_start: i32,
    pub row_end: i32,
    /// The formula of the run's first cell, in A1 terms (e.g. `=B2*C2`).
    pub anchor_formula: String,
    /// Rows inside `[row_start, row_end]` holding a *different* formula.
    pub breaks: Vec<i32>,
}

impl FillRun {
    pub fn len(&self) -> i32 {
        self.row_end - self.row_start + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn range(&self) -> Range {
        Range::new(
            CellRef { row: self.row_start, col: self.col },
            CellRef { row: self.row_end, col: self.col },
        )
    }
}

/// Detect fill runs in one column of a range. Only runs of at least
/// `min_len` cells are reported; shorter formula groups stay individual.
pub fn column_fill_runs(book: &Book, sheet: u32, col: i32, range: Range, min_len: i32) -> Vec<FillRun> {
    // Collect (row, formula_index) for formula cells in this column, sorted.
    let mut rows: Vec<(i32, i32)> = Vec::new();
    book.for_each_cell(sheet, |row, c, cell| {
        if c == col && range.contains(row, col) {
            if let Some(f) = cell.get_formula() {
                rows.push((row, f));
            }
        }
    });
    rows.sort_unstable();
    if rows.is_empty() {
        return vec![];
    }

    // Majority index over the column: the dominant formula defines the run;
    // interspersed different indices become breaks rather than run splits,
    // as long as the dominant index covers most of the span.
    let mut runs: Vec<FillRun> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let (start_row, idx) = rows[i];
        // Extend while contiguous-ish: allow different indices inside, but the
        // run ends when we hit a gap of >1 row of *no formula at all*.
        let mut j = i;
        let mut breaks = Vec::new();
        let mut last_row = start_row;
        let mut member_count = 1; // cells with the dominant index
        while j + 1 < rows.len() {
            let (next_row, next_idx) = rows[j + 1];
            if next_row - last_row > 1 {
                break;
            }
            if next_idx == idx {
                member_count += 1;
            } else {
                breaks.push(next_row);
            }
            last_row = next_row;
            j += 1;
        }
        // A run must be dominated by its index; otherwise fall back to
        // reporting the first group alone.
        let span = last_row - start_row + 1;
        if span >= min_len && member_count * 2 > span {
            let anchor_formula = book
                .formula(sheet, start_row, col)
                .ok()
                .flatten()
                .unwrap_or_default();
            runs.push(FillRun {
                sheet,
                col,
                row_start: start_row,
                row_end: last_row,
                anchor_formula,
                breaks,
            });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book_with_fill() -> Book {
        let mut book = Book::new_empty("t").unwrap();
        book.batch(|b| {
            for row in 1..=50 {
                b.set_input(0, row, 1, &format!("{row}"))?; // A
                b.set_input(0, row, 2, "3")?; // B
                if row == 25 {
                    b.set_input(0, row, 3, "=A25*B25*2")?; // deviation
                } else {
                    b.set_input(0, row, 3, &format!("=A{row}*B{row}"))?; // C fill
                }
            }
            Ok(())
        })
        .unwrap();
        book
    }

    #[test]
    fn detects_uniform_fill_with_break() {
        let book = book_with_fill();
        let range = book.used_range(0).unwrap();
        let runs = column_fill_runs(&book, 0, 3, range, 4);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!((run.row_start, run.row_end), (1, 50));
        assert_eq!(run.anchor_formula, "=A1*B1");
        assert_eq!(run.breaks, vec![25]);
    }

    #[test]
    fn no_formulas_no_runs() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "5").unwrap();
        let runs = column_fill_runs(&book, 0, 1, book.used_range(0).unwrap(), 4);
        assert!(runs.is_empty());
    }
}
