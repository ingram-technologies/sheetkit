//! Region detection: find the tables living inside a sheet, infer headers and
//! per-column types/statistics. This is what lets an agent address data as
//! `orders` instead of guessing `A1:F9871`, and what powers the aggregated view.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::addr::{CellRef, Range};
use crate::book::{format_number, Book, Value};
use crate::fills::{column_fill_runs, FillRun};

/// How many empty rows split two regions.
const ROW_GAP: i32 = 2;
/// Distinct-value tracking cap per column.
const DISTINCT_CAP: usize = 2000;
/// Cells sampled per column for date-format sniffing.
const DATE_SNIFF_SAMPLE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    Number,
    Text,
    Date,
    Bool,
    Mixed,
    Empty,
}

impl Dtype {
    pub fn label(&self) -> &'static str {
        match self {
            Dtype::Number => "number",
            Dtype::Text => "text",
            Dtype::Date => "date",
            Dtype::Bool => "bool",
            Dtype::Mixed => "mixed",
            Dtype::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sortedness {
    Asc,
    Desc,
    Unsorted,
    NotApplicable,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub col: i32,
    pub header: Option<String>,
    pub dtype: Dtype,
    pub non_empty: i32,
    pub body_rows: i32,
    /// Distinct display values; `None` when the cap was exceeded.
    pub distinct: Option<usize>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Formatted min/max (meaningful for date columns).
    pub min_display: Option<String>,
    pub max_display: Option<String>,
    pub sorted: Sortedness,
    pub fill: Option<FillRun>,
    /// Up to a handful of most frequent values, for low-cardinality columns.
    pub top_values: Vec<(String, usize)>,
    pub error_count: i32,
}

#[derive(Debug, Clone)]
pub struct Region {
    /// Auto-assigned (`table1`) or taken from an Excel table definition.
    pub name: String,
    pub sheet: u32,
    pub sheet_name: String,
    pub range: Range,
    pub header_row: Option<i32>,
    pub columns: Vec<ColumnInfo>,
}

impl Region {
    /// First body (non-header) row.
    pub fn body_start(&self) -> i32 {
        match self.header_row {
            Some(h) => h + 1,
            None => self.range.start.row,
        }
    }

    pub fn body_rows(&self) -> i32 {
        self.range.end.row - self.body_start() + 1
    }

    pub fn header_of(&self, col: i32) -> Option<&str> {
        self.columns
            .iter()
            .find(|c| c.col == col)
            .and_then(|c| c.header.as_deref())
    }

    /// Find a column by header name (case-insensitive) or letter.
    pub fn column_by_key(&self, key: &str) -> Option<i32> {
        if let Some(c) = self
            .columns
            .iter()
            .find(|c| c.header.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(key)))
        {
            return Some(c.col);
        }
        crate::addr::column_to_number(&key.to_ascii_uppercase()).ok().filter(|c| {
            *c >= self.range.start.col && *c <= self.range.end.col
        })
    }
}

/// Detect regions on one sheet. Excel table definitions seed regions with
/// their names; everything else gets `table<N>` in reading order.
pub fn detect(book: &Book, sheet: u32) -> Vec<Region> {
    let Some(_used) = book.used_range(sheet) else {
        return vec![];
    };
    let sheet_name = book
        .sheet_names()
        .get(sheet as usize)
        .cloned()
        .unwrap_or_default();

    // Row occupancy map: row -> (min_col, max_col, count)
    let mut occupancy: BTreeMap<i32, (i32, i32)> = BTreeMap::new();
    book.for_each_cell(sheet, |row, col, _cell| {
        let e = occupancy.entry(row).or_insert((col, col));
        e.0 = e.0.min(col);
        e.1 = e.1.max(col);
    });

    // Split occupied rows into blocks separated by >= ROW_GAP empty rows.
    let mut blocks: Vec<(i32, i32, i32, i32)> = Vec::new(); // (r1, r2, c1, c2)
    let mut current: Option<(i32, i32, i32, i32)> = None;
    for (&row, &(c1, c2)) in &occupancy {
        match current {
            Some((r1, r2, bc1, bc2)) if row - r2 <= ROW_GAP => {
                current = Some((r1, row, bc1.min(c1), bc2.max(c2)));
            }
            Some(done) => {
                blocks.push(done);
                current = Some((row, row, c1, c2));
            }
            None => current = Some((row, row, c1, c2)),
        }
    }
    if let Some(done) = current {
        blocks.push(done);
    }

    // Excel table names by top-left cell.
    let mut table_names: HashMap<(i32, i32), String> = HashMap::new();
    for (name, tsheet, reference) in book.tables() {
        if tsheet.eq_ignore_ascii_case(&sheet_name) {
            if let Some(t) = crate::addr::parse_target(&reference) {
                if let crate::addr::TargetKind::Range(r) = t.kind {
                    table_names.insert((r.start.row, r.start.col), name);
                }
            }
        }
    }

    let mut regions = Vec::new();
    for (i, &(r1, r2, c1, c2)) in blocks.iter().enumerate() {
        let range = Range::new(CellRef { row: r1, col: c1 }, CellRef { row: r2, col: c2 });
        let name = table_names
            .get(&(r1, c1))
            .cloned()
            .unwrap_or_else(|| format!("table{}", i + 1));
        regions.push(analyze_region(book, sheet, &sheet_name, name, range));
    }
    regions
}

/// Detect regions across all sheets, returning a name → (sheet, range) map for
/// target resolution alongside the full region list.
pub fn detect_all(book: &Book) -> (Vec<Region>, HashMap<String, (u32, Range)>) {
    let mut all = Vec::new();
    let mut index = HashMap::new();
    let mut counter = 0usize;
    for sheet in 0..book.sheet_count() {
        for mut region in detect(book, sheet) {
            // Re-number auto names across the workbook so they stay unique.
            if region.name.starts_with("table") {
                counter += 1;
                region.name = format!("table{counter}");
            }
            index.insert(region.name.to_lowercase(), (region.sheet, region.range));
            all.push(region);
        }
    }
    (all, index)
}

fn analyze_region(book: &Book, sheet: u32, sheet_name: &str, name: String, range: Range) -> Region {
    // Header heuristic: the first row is a header when every non-empty cell in
    // it is text and the row below contains at least one non-text value.
    let first_row = range.start.row;
    let header_row = {
        let mut first_all_text = true;
        let mut first_non_empty = 0;
        let mut second_has_non_text = false;
        for col in range.start.col..=range.end.col {
            match book.value(sheet, first_row, col) {
                Value::Text(_) => first_non_empty += 1,
                Value::Empty => {}
                _ => {
                    first_all_text = false;
                }
            }
            if range.height() > 1 {
                match book.value(sheet, first_row + 1, col) {
                    Value::Text(_) | Value::Empty => {}
                    _ => second_has_non_text = true,
                }
            }
        }
        if first_all_text && first_non_empty > 0 && (range.height() == 1 || second_has_non_text) {
            if range.height() > 1 {
                Some(first_row)
            } else {
                None
            }
        } else {
            None
        }
    };

    let body_start = header_row.map_or(range.start.row, |h| h + 1);
    let body_rows = range.end.row - body_start + 1;

    let mut columns = Vec::new();
    for col in range.start.col..=range.end.col {
        columns.push(analyze_column(book, sheet, col, body_start, range.end.row, header_row, range, body_rows));
    }

    Region {
        name,
        sheet,
        sheet_name: sheet_name.to_string(),
        range,
        header_row,
        columns,
    }
}

#[allow(clippy::too_many_arguments)]
fn analyze_column(
    book: &Book,
    sheet: u32,
    col: i32,
    body_start: i32,
    body_end: i32,
    header_row: Option<i32>,
    region_range: Range,
    body_rows: i32,
) -> ColumnInfo {
    let header = header_row.and_then(|h| match book.value(sheet, h, col) {
        Value::Text(s) => Some(s),
        _ => None,
    });

    let mut non_empty = 0;
    let mut numbers = 0;
    let mut texts = 0;
    let mut bools = 0;
    let mut errors = 0;
    let mut min: Option<f64> = None;
    let mut max: Option<f64> = None;
    let mut distinct: BTreeSet<String> = BTreeSet::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut over_cap = false;
    let mut prev_num: Option<f64> = None;
    let mut prev_text: Option<String> = None;
    let mut asc = true;
    let mut desc = true;
    let mut comparable = 0;

    // Values in row order for sortedness.
    let mut cells: Vec<(i32, Value)> = Vec::new();
    book.for_each_cell(sheet, |row, c, cell| {
        if c == col && row >= body_start && row <= body_end {
            cells.push((row, crate::book::cell_to_value(cell, book.shared_strings())));
        }
    });
    cells.sort_unstable_by_key(|(row, _)| *row);

    for (_row, v) in &cells {
        if v.is_empty() {
            continue;
        }
        non_empty += 1;
        let display = v.display();
        if !over_cap {
            distinct.insert(display.clone());
            if distinct.len() > DISTINCT_CAP {
                over_cap = true;
            }
        }
        *counts.entry(display).or_insert(0) += 1;
        match v {
            Value::Number(n) => {
                numbers += 1;
                min = Some(min.map_or(*n, |m| m.min(*n)));
                max = Some(max.map_or(*n, |m| m.max(*n)));
                if let Some(p) = prev_num {
                    if *n < p {
                        asc = false;
                    }
                    if *n > p {
                        desc = false;
                    }
                    comparable += 1;
                }
                prev_num = Some(*n);
            }
            Value::Text(s) => {
                texts += 1;
                if let Some(p) = &prev_text {
                    let ord = s.to_lowercase().cmp(&p.to_lowercase());
                    if ord == std::cmp::Ordering::Less {
                        asc = false;
                    }
                    if ord == std::cmp::Ordering::Greater {
                        desc = false;
                    }
                    comparable += 1;
                }
                prev_text = Some(s.clone());
            }
            Value::Bool(_) => bools += 1,
            Value::Error(_) => errors += 1,
            Value::Empty => {}
        }
    }

    // Date sniffing: a numeric column whose cell formats look like dates.
    let mut is_date = false;
    if numbers > 0 && numbers >= texts {
        let mut sniffed = 0;
        for (row, v) in &cells {
            if sniffed >= DATE_SNIFF_SAMPLE {
                break;
            }
            if matches!(v, Value::Number(_)) {
                sniffed += 1;
                let fmt = book.num_fmt(sheet, *row, col).to_lowercase();
                if fmt.contains('y') || (fmt.contains('d') && fmt.contains('m')) {
                    is_date = true;
                } else {
                    is_date = false;
                    break;
                }
            }
        }
        if sniffed == 0 {
            is_date = false;
        }
    }

    let dtype = if non_empty == 0 {
        Dtype::Empty
    } else if is_date && numbers * 10 >= non_empty * 9 {
        Dtype::Date
    } else if numbers == non_empty - errors && numbers > 0 {
        Dtype::Number
    } else if texts == non_empty - errors && texts > 0 {
        Dtype::Text
    } else if bools == non_empty - errors && bools > 0 {
        Dtype::Bool
    } else {
        Dtype::Mixed
    };

    let sorted = if comparable < 2 || non_empty < 3 {
        Sortedness::NotApplicable
    } else if asc {
        Sortedness::Asc
    } else if desc {
        Sortedness::Desc
    } else {
        Sortedness::Unsorted
    };

    // Formatted min/max for dates.
    let (min_display, max_display) = if dtype == Dtype::Date && min.is_some() {
        let find_row = |target: f64| {
            cells
                .iter()
                .find(|(_, v)| matches!(v, Value::Number(n) if *n == target))
                .map(|(row, _)| *row)
        };
        (
            min.and_then(find_row).map(|r| book.formatted_value(sheet, r, col)),
            max.and_then(find_row).map(|r| book.formatted_value(sheet, r, col)),
        )
    } else {
        (min.map(format_number), max.map(format_number))
    };

    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(4);
    if over_cap || distinct.len() > 8 {
        // High cardinality: top values are noise, drop them.
        top.clear();
    }

    let fill = column_fill_runs(book, sheet, col, region_range, 4)
        .into_iter()
        .max_by_key(|r| r.len());

    ColumnInfo {
        col,
        header,
        dtype,
        non_empty,
        body_rows,
        distinct: if over_cap { None } else { Some(distinct.len()) },
        min,
        max,
        min_display,
        max_display,
        sorted,
        fill,
        top_values: top,
        error_count: errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_book() -> Book {
        let mut book = Book::new_empty("t").unwrap();
        book.batch(|b| {
            b.set_input(0, 1, 1, "Item")?;
            b.set_input(0, 1, 2, "Qty")?;
            b.set_input(0, 1, 3, "Price")?;
            b.set_input(0, 1, 4, "Total")?;
            let items = ["Ape", "Bee", "Cat", "Dog", "Eel"];
            for (i, item) in items.iter().enumerate() {
                let row = i as i32 + 2;
                b.set_input(0, row, 1, item)?;
                b.set_input(0, row, 2, &format!("{}", (i + 1) * 2))?;
                b.set_input(0, row, 3, "1.5")?;
                b.set_input(0, row, 4, &format!("=B{row}*C{row}"))?;
            }
            // A second region further down.
            b.set_input(0, 10, 1, "note")?;
            Ok(())
        })
        .unwrap();
        book
    }

    #[test]
    fn detects_two_regions_with_header() {
        let book = sample_book();
        let regions = detect(&book, 0);
        assert_eq!(regions.len(), 2);
        let r = &regions[0];
        assert_eq!(r.range.a1(), "A1:D6");
        assert_eq!(r.header_row, Some(1));
        assert_eq!(r.header_of(2), Some("Qty"));
        assert_eq!(r.body_rows(), 5);
        assert_eq!(regions[1].range.a1(), "A10");
    }

    #[test]
    fn column_stats() {
        let book = sample_book();
        let regions = detect(&book, 0);
        let qty = &regions[0].columns[1];
        assert_eq!(qty.dtype, Dtype::Number);
        assert_eq!(qty.min, Some(2.0));
        assert_eq!(qty.max, Some(10.0));
        assert_eq!(qty.sorted, Sortedness::Asc);
        let item = &regions[0].columns[0];
        assert_eq!(item.dtype, Dtype::Text);
        assert_eq!(item.distinct, Some(5));
        // Total column: uniform fill detected
        let total = &regions[0].columns[3];
        assert!(total.fill.is_some());
        assert_eq!(total.fill.as_ref().unwrap().anchor_formula, "=B2*C2");
    }

    #[test]
    fn column_by_key_matches_header_and_letter() {
        let book = sample_book();
        let regions = detect(&book, 0);
        assert_eq!(regions[0].column_by_key("qty"), Some(2));
        assert_eq!(regions[0].column_by_key("C"), Some(3));
        assert_eq!(regions[0].column_by_key("nope"), None);
    }
}
