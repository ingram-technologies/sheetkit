//! `Book` wraps an IronCalc [`UserModel`] with the addressing, I/O and
//! inspection helpers the rest of the toolkit builds on.

use std::collections::HashMap;
#[cfg(feature = "xlsx")]
use std::path::Path;

#[cfg(feature = "xlsx")]
use ironcalc::{export, import};
use ironcalc_base::cell::CellValue;
use ironcalc_base::expressions::types::Area;
use ironcalc_base::types::Cell;
use ironcalc_base::UserModel;

use crate::addr::{display_sheet, CellRef, Range, Target, TargetKind};
use crate::{Error, Result};

const LOCALE: &str = "en";
const TIMEZONE: &str = "UTC";
const LANGUAGE: &str = "en";

/// A target resolved against a concrete workbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub sheet_index: u32,
    pub sheet_name: String,
    pub range: Range,
    /// Set when the target came from a named region or defined name.
    pub via: Option<String>,
}

impl Resolved {
    pub fn qualified(&self) -> String {
        format!("{}!{}", display_sheet(&self.sheet_name), self.range.a1())
    }
}

/// The evaluated value of a cell, reduced to what the text encodings need.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Empty,
    Number(f64),
    Text(String),
    Bool(bool),
    Error(String),
}

impl Value {
    pub fn is_empty(&self) -> bool {
        matches!(self, Value::Empty)
    }

    /// Compact display form: strings quoted, numbers bare, errors as-is.
    pub fn display(&self) -> String {
        match self {
            Value::Empty => String::new(),
            Value::Number(n) => format_number(*n),
            Value::Text(s) => format!("{s:?}"),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Error(e) => e.clone(),
        }
    }
}

/// Render a number the way a human would type it: integers bare, and float
/// representation dust (12877.689999999999) rounded away by keeping 12
/// significant digits.
pub fn format_number(n: f64) -> String {
    let rounded = if n.is_finite() && n != 0.0 {
        let magnitude = n.abs().log10().floor();
        let factor = 10f64.powf(11.0 - magnitude);
        if factor.is_finite() && (n * factor).abs() < 9e15 {
            (n * factor).round() / factor
        } else {
            n
        }
    } else {
        n
    };
    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

pub struct Book {
    um: UserModel<'static>,
}

impl Book {
    pub fn new_empty(name: &str) -> Result<Book> {
        // The engine constructor ties its lifetime to every &str param; use a
        // static placeholder and set the real (owned) name afterwards.
        let mut um = UserModel::new_empty("workbook", LOCALE, TIMEZONE, LANGUAGE)?;
        um.set_name(name);
        Ok(Book { um })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Book> {
        let mut um = UserModel::from_bytes(bytes, LANGUAGE)?;
        um.evaluate();
        Ok(Book { um })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.um.to_bytes()
    }

    /// Drain the engine's pending diff queue (bitcode blob). Replicas running
    /// the same engine version apply it with [`Book::apply_diffs`]; anything
    /// else must treat it as opaque. Empty when nothing changed.
    pub fn flush_diffs(&mut self) -> Vec<u8> {
        self.um.flush_send_queue()
    }

    /// Apply a diff blob produced by another instance's [`Book::flush_diffs`].
    pub fn apply_diffs(&mut self, blob: &[u8]) -> Result<()> {
        self.um.apply_external_diffs(blob)?;
        Ok(())
    }

    /// Force a full re-evaluation (needed after replaying diff blobs).
    pub fn evaluate(&mut self) {
        self.um.evaluate();
    }

    /// Number of non-empty cells across all sheets — the memory driver, used
    /// for admission control.
    pub fn non_empty_count(&self) -> u64 {
        let mut n = 0u64;
        for sheet in 0..self.sheet_names().len() as u32 {
            self.for_each_cell(sheet, |_, _, _| n += 1);
        }
        n
    }

    /// Import a workbook from xlsx bytes (no file needed).
    #[cfg(feature = "xlsx")]
    pub fn from_xlsx_bytes(bytes: &[u8], name: &str) -> Result<Book> {
        let workbook = import::load_from_xlsx_bytes(bytes, name, LOCALE, TIMEZONE)
            .map_err(|e| Error::from(format!("failed to read xlsx bytes: {e}")))?;
        let model = ironcalc_base::Model::from_workbook(workbook, LANGUAGE)
            .map_err(|e| Error::from(format!("failed to build model: {e}")))?;
        let mut um = UserModel::from_model(model);
        um.evaluate();
        Ok(Book { um })
    }

    /// Export the workbook as xlsx bytes.
    #[cfg(feature = "xlsx")]
    pub fn to_xlsx_bytes(&self) -> Result<Vec<u8>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let out = export::save_xlsx_to_writer(self.um.get_model(), cursor)
            .map_err(|e| Error::from(format!("failed to write xlsx: {e}")))?;
        Ok(out.into_inner())
    }

    /// Import a workbook from CSV text.
    pub fn from_csv_str(csv: &str, name: &str) -> Result<Book> {
        let mut book = Book::new_empty(name)?;
        book.paste_csv(0, CellRef { row: 1, col: 1 }, csv)?;
        Ok(book)
    }

    /// Open a workbook from a file path; the format is chosen by extension
    /// (`.xlsx`, `.ic`/`.icalc`, `.csv` — anything else is an error).
    #[cfg(feature = "xlsx")]
    pub fn open(path: &str) -> Result<Book> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "xlsx" | "xlsm" => {
                let model = import::load_from_xlsx(path, LOCALE, TIMEZONE, LANGUAGE)
                    .map_err(|e| Error::from(format!("failed to open {path}: {e}")))?;
                let mut um = UserModel::from_model(model);
                um.evaluate();
                Ok(Book { um })
            }
            "ic" | "icalc" => {
                let model = import::load_from_icalc(path, LANGUAGE)
                    .map_err(|e| Error::from(format!("failed to open {path}: {e}")))?;
                let mut um = UserModel::from_model(model);
                um.evaluate();
                Ok(Book { um })
            }
            "csv" => {
                let csv = std::fs::read_to_string(path)
                    .map_err(|e| Error::from(format!("failed to read {path}: {e}")))?;
                let name = Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workbook");
                Book::from_csv_str(&csv, name)
            }
            other => Err(Error::from(format!(
                "unsupported file extension {other:?} (expected xlsx, ic or csv)"
            ))),
        }
    }

    /// Save to a file path; format by extension. Refuses to overwrite unless asked.
    #[cfg(feature = "xlsx")]
    pub fn save(&self, path: &str, overwrite: bool) -> Result<()> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if Path::new(path).exists() {
            if !overwrite {
                return Err(Error::from(format!(
                    "{path} exists; pass overwrite to replace it"
                )));
            }
            std::fs::remove_file(path)
                .map_err(|e| Error::from(format!("failed to replace {path}: {e}")))?;
        }
        match ext.as_str() {
            "xlsx" => export::save_to_xlsx(self.um.get_model(), path)
                .map_err(|e| Error::from(format!("failed to save {path}: {e}"))),
            "ic" | "icalc" => export::save_to_icalc(self.um.get_model(), path)
                .map_err(|e| Error::from(format!("failed to save {path}: {e}"))),
            "csv" => {
                let csv = self.to_csv(0)?;
                std::fs::write(path, csv)
                    .map_err(|e| Error::from(format!("failed to save {path}: {e}")))
            }
            other => Err(Error::from(format!(
                "unsupported file extension {other:?} (expected xlsx, ic or csv)"
            ))),
        }
    }

    pub fn name(&self) -> String {
        self.um.get_name()
    }

    // ---- sheets ----------------------------------------------------------

    pub fn sheet_names(&self) -> Vec<String> {
        self.um
            .get_worksheets_properties()
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    pub fn sheet_index(&self, name: &str) -> Option<u32> {
        self.sheet_names()
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .map(|i| i as u32)
    }

    pub fn sheet_count(&self) -> u32 {
        self.sheet_names().len() as u32
    }

    pub fn add_sheet(&mut self, name: &str) -> Result<()> {
        self.um.new_sheet()?;
        let idx = self.sheet_count() - 1;
        self.um.rename_sheet(idx, name)?;
        Ok(())
    }

    pub fn rename_sheet(&mut self, index: u32, name: &str) -> Result<()> {
        self.um.rename_sheet(index, name)?;
        Ok(())
    }

    pub fn delete_sheet(&mut self, index: u32) -> Result<()> {
        self.um.delete_sheet(index)?;
        Ok(())
    }

    /// The rectangle actually containing data, or `None` for an empty sheet.
    pub fn used_range(&self, sheet: u32) -> Option<Range> {
        let ws = self.um.get_model().workbook.worksheet(sheet).ok()?;
        let mut min_row = i32::MAX;
        let mut max_row = i32::MIN;
        let mut min_col = i32::MAX;
        let mut max_col = i32::MIN;
        for (&row, cols) in &ws.sheet_data {
            for (&col, cell) in cols {
                if matches!(cell, Cell::EmptyCell { .. }) {
                    continue;
                }
                min_row = min_row.min(row);
                max_row = max_row.max(row);
                min_col = min_col.min(col);
                max_col = max_col.max(col);
            }
        }
        if min_row == i32::MAX {
            None
        } else {
            Some(Range::new(
                CellRef { row: min_row, col: min_col },
                CellRef { row: max_row, col: max_col },
            ))
        }
    }

    // ---- cell reads ------------------------------------------------------

    /// Raw cell input: `=SUM(A1:A3)` for formulas, the literal otherwise, `""` if empty.
    pub fn content(&self, sheet: u32, row: i32, col: i32) -> Result<String> {
        Ok(self.um.get_cell_content(sheet, row, col)?)
    }

    /// Apply a number format to a range (e.g. `yyyy-mm-dd` to make serial
    /// numbers display as dates).
    pub fn set_num_fmt(&mut self, sheet: u32, range: Range, fmt: &str) -> Result<()> {
        self.um
            .update_range_style(&to_area(sheet, range), "num_fmt", fmt)?;
        Ok(())
    }

    /// The formula at a cell (with `=`), if any.
    pub fn formula(&self, sheet: u32, row: i32, col: i32) -> Result<Option<String>> {
        Ok(self.um.get_model().get_cell_formula(sheet, row, col)?)
    }

    pub fn value(&self, sheet: u32, row: i32, col: i32) -> Value {
        let model = self.um.get_model();
        let Ok(ws) = model.workbook.worksheet(sheet) else {
            return Value::Empty;
        };
        match ws.cell(row, col) {
            None => Value::Empty,
            Some(cell) => cell_to_value(cell, &model.workbook.shared_strings),
        }
    }

    /// The value formatted through the cell's number format (dates, currencies…).
    pub fn formatted_value(&self, sheet: u32, row: i32, col: i32) -> String {
        self.um
            .get_formatted_cell_value(sheet, row, col)
            .unwrap_or_default()
    }

    /// Index of the shared formula for a cell, if it holds a formula.
    pub fn formula_index(&self, sheet: u32, row: i32, col: i32) -> Option<i32> {
        let ws = self.um.get_model().workbook.worksheet(sheet).ok()?;
        ws.cell(row, col).and_then(|c| c.get_formula())
    }

    /// Visit every non-empty cell of a sheet: `(row, col, &Cell)`.
    pub fn for_each_cell<F: FnMut(i32, i32, &Cell)>(&self, sheet: u32, mut f: F) {
        if let Ok(ws) = self.um.get_model().workbook.worksheet(sheet) {
            for (&row, cols) in &ws.sheet_data {
                for (&col, cell) in cols {
                    if !matches!(cell, Cell::EmptyCell { .. }) {
                        f(row, col, cell);
                    }
                }
            }
        }
    }

    pub fn shared_strings(&self) -> &[String] {
        &self.um.get_model().workbook.shared_strings
    }

    /// The number format applied to a cell (`"general"` when unstyled).
    pub fn num_fmt(&self, sheet: u32, row: i32, col: i32) -> String {
        self.um
            .get_cell_style(sheet, row, col)
            .map(|s| s.num_fmt)
            .unwrap_or_else(|_| "general".to_string())
    }

    /// Excel tables defined in the workbook: `(display name, sheet name, reference)`.
    pub fn tables(&self) -> Vec<(String, String, String)> {
        self.um
            .get_model()
            .workbook
            .tables
            .values()
            .map(|t| (t.display_name.clone(), t.sheet_name.clone(), t.reference.clone()))
            .collect()
    }

    // ---- mutations -------------------------------------------------------

    pub fn set_input(&mut self, sheet: u32, row: i32, col: i32, value: &str) -> Result<()> {
        self.um.set_user_input(sheet, row, col, value)?;
        Ok(())
    }

    /// Run several mutations as one evaluation batch (one recalc at the end).
    pub fn batch<F: FnOnce(&mut Book) -> Result<()>>(&mut self, f: F) -> Result<()> {
        self.um.pause_evaluation();
        let result = f(self);
        self.um.resume_evaluation();
        self.um.evaluate();
        result
    }

    pub fn clear_contents(&mut self, sheet: u32, range: Range) -> Result<()> {
        self.um.range_clear_contents(&to_area(sheet, range))?;
        Ok(())
    }

    pub fn clear_formatting(&mut self, sheet: u32, range: Range) -> Result<()> {
        self.um.range_clear_formatting(&to_area(sheet, range))?;
        Ok(())
    }

    pub fn clear_all(&mut self, sheet: u32, range: Range) -> Result<()> {
        self.um.range_clear_all(&to_area(sheet, range))?;
        Ok(())
    }

    pub fn auto_fill_rows(&mut self, sheet: u32, source: Range, to_row: i32) -> Result<()> {
        self.um.auto_fill_rows(&to_area(sheet, source), to_row)?;
        Ok(())
    }

    pub fn auto_fill_columns(&mut self, sheet: u32, source: Range, to_col: i32) -> Result<()> {
        self.um.auto_fill_columns(&to_area(sheet, source), to_col)?;
        Ok(())
    }

    pub fn insert_rows(&mut self, sheet: u32, row: i32, count: i32) -> Result<()> {
        self.um.insert_rows(sheet, row, count)?;
        Ok(())
    }

    pub fn insert_columns(&mut self, sheet: u32, col: i32, count: i32) -> Result<()> {
        self.um.insert_columns(sheet, col, count)?;
        Ok(())
    }

    pub fn delete_rows(&mut self, sheet: u32, row: i32, count: i32) -> Result<()> {
        self.um.delete_rows(sheet, row, count)?;
        Ok(())
    }

    pub fn delete_columns(&mut self, sheet: u32, col: i32, count: i32) -> Result<()> {
        self.um.delete_columns(sheet, col, count)?;
        Ok(())
    }

    /// Paste comma-separated values with `at` as the top-left corner.
    /// (The engine's own `paste_csv_string` is tab-delimited clipboard paste.)
    pub fn paste_csv(&mut self, sheet: u32, at: CellRef, csv: &str) -> Result<()> {
        let rows = parse_csv(csv);
        self.batch(|b| {
            for (i, cells) in rows.iter().enumerate() {
                for (j, value) in cells.iter().enumerate() {
                    if !value.is_empty() {
                        b.set_input(sheet, at.row + i as i32, at.col + j as i32, value)?;
                    }
                }
            }
            Ok(())
        })
    }

    pub fn undo(&mut self) -> Result<()> {
        self.um.undo()?;
        Ok(())
    }

    pub fn redo(&mut self) -> Result<()> {
        self.um.redo()?;
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        self.um.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.um.can_redo()
    }

    // ---- defined names ---------------------------------------------------

    /// `(name, scope sheet, formula)` for every defined name.
    pub fn defined_names(&self) -> Vec<(String, Option<u32>, String)> {
        self.um.get_defined_name_list()
    }

    pub fn define_name(&mut self, name: &str, formula: &str) -> Result<()> {
        self.um.new_defined_name(name, None, formula)?;
        Ok(())
    }

    pub fn delete_defined_name(&mut self, name: &str) -> Result<()> {
        self.um.delete_defined_name(name, None)?;
        Ok(())
    }

    // ---- target resolution -------------------------------------------------

    /// Resolve a parsed target against this workbook. `default_sheet` scopes
    /// unqualified references. `regions` supplies named-region resolution.
    pub fn resolve(
        &self,
        target: &Target,
        default_sheet: u32,
        regions: &HashMap<String, (u32, Range)>,
    ) -> Result<Resolved> {
        let sheet_index = match &target.sheet {
            Some(name) => self
                .sheet_index(name)
                .ok_or_else(|| Error::from(format!("no sheet named {name:?}")))?,
            None => default_sheet,
        };
        let make = |sheet_index: u32, range: Range, via: Option<String>| -> Result<Resolved> {
            let sheet_name = self
                .sheet_names()
                .get(sheet_index as usize)
                .cloned()
                .ok_or_else(|| Error::from(format!("no sheet at index {sheet_index}")))?;
            Ok(Resolved { sheet_index, sheet_name, range, via })
        };
        match &target.kind {
            TargetKind::Range(r) => make(sheet_index, *r, None),
            TargetKind::Cols { start, end } => {
                let used = self.used_range(sheet_index).ok_or_else(|| {
                    Error::from("cannot use a whole-column target on an empty sheet")
                })?;
                make(
                    sheet_index,
                    Range::new(
                        CellRef { row: used.start.row, col: *start },
                        CellRef { row: used.end.row, col: *end },
                    ),
                    None,
                )
            }
            TargetKind::Rows { start, end } => {
                let used = self.used_range(sheet_index).ok_or_else(|| {
                    Error::from("cannot use a whole-row target on an empty sheet")
                })?;
                make(
                    sheet_index,
                    Range::new(
                        CellRef { row: *start, col: used.start.col },
                        CellRef { row: *end, col: used.end.col },
                    ),
                    None,
                )
            }
            TargetKind::Ident(name) => {
                // 1. named region
                let lower = name.to_lowercase();
                if let Some((sheet, range)) = regions.get(&lower) {
                    return make(*sheet, *range, Some(name.clone()));
                }
                // 2. defined name (only plain `Sheet!A1:B2` formulas resolve)
                for (dn, _scope, formula) in self.defined_names() {
                    if dn.eq_ignore_ascii_case(name) {
                        if let Some(t) = crate::addr::parse_target(&formula) {
                            if !matches!(t.kind, TargetKind::Ident(_)) {
                                return self.resolve(&t, default_sheet, regions);
                            }
                        }
                        return Err(Error::from(format!(
                            "defined name {name:?} = {formula:?} is not a plain range"
                        )));
                    }
                }
                // 3. sheet name → whole used range
                if let Some(idx) = self.sheet_index(name) {
                    let range = self
                        .used_range(idx)
                        .unwrap_or(Range::cell(CellRef { row: 1, col: 1 }));
                    return make(idx, range, None);
                }
                Err(Error::from(format!(
                    "cannot resolve {name:?} (not a region, defined name, or sheet)"
                )))
            }
        }
    }

    // ---- csv export --------------------------------------------------------

    pub fn to_csv(&self, sheet: u32) -> Result<String> {
        let Some(used) = self.used_range(sheet) else {
            return Ok(String::new());
        };
        let mut out = String::new();
        for row in 1..=used.end.row {
            let mut line: Vec<String> = Vec::with_capacity(used.end.col as usize);
            for col in 1..=used.end.col {
                let v = self.formatted_value(sheet, row, col);
                line.push(csv_quote(&v));
            }
            out.push_str(&line.join(","));
            out.push('\n');
        }
        Ok(out)
    }
}

/// Minimal RFC-4180 CSV parsing: quoted fields, escaped quotes, CRLF.
fn parse_csv(csv: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = csv.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            ',' => {
                row.push(std::mem::take(&mut field));
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn csv_quote(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub(crate) fn to_area(sheet: u32, range: Range) -> Area {
    Area {
        sheet,
        row: range.start.row,
        column: range.start.col,
        width: range.width(),
        height: range.height(),
    }
}

pub(crate) fn cell_to_value(cell: &Cell, shared_strings: &[String]) -> Value {
    use ironcalc_base::language::get_language;
    let language = get_language(LANGUAGE).expect("en language");
    // Text cells stay text even when they read like "#N/A"; only error cells
    // and error formula/spill results classify as errors.
    use ironcalc_base::types::{FormulaValue, SpillValue};
    let is_error_kind = matches!(
        cell,
        Cell::ErrorCell { .. }
            | Cell::CellFormula { v: FormulaValue::Error { .. }, .. }
            | Cell::ArrayFormula { v: FormulaValue::Error { .. }, .. }
            | Cell::SpillCell { v: SpillValue::Error(_), .. }
    );
    match cell.value(shared_strings, language) {
        CellValue::None => Value::Empty,
        CellValue::Number(n) => Value::Number(n),
        CellValue::Boolean(b) => Value::Bool(b),
        CellValue::String(s) => {
            if is_error_kind {
                Value::Error(s)
            } else {
                Value::Text(s)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_values_and_formulas() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "1").unwrap();
        book.set_input(0, 2, 1, "2").unwrap();
        book.set_input(0, 3, 1, "=A1+A2").unwrap();
        assert_eq!(book.value(0, 3, 1), Value::Number(3.0));
        assert_eq!(book.formula(0, 3, 1).unwrap().as_deref(), Some("=A1+A2"));
        assert_eq!(book.content(0, 3, 1).unwrap(), "=A1+A2");
        assert_eq!(book.used_range(0).unwrap().a1(), "A1:A3");
    }

    #[test]
    fn errors_are_values() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "=1/0").unwrap();
        assert_eq!(book.value(0, 1, 1), Value::Error("#DIV/0!".into()));
    }

    #[test]
    fn batch_evaluates_once_at_end() {
        let mut book = Book::new_empty("t").unwrap();
        book.batch(|b| {
            for row in 1..=100 {
                b.set_input(0, row, 1, &row.to_string())?;
            }
            b.set_input(0, 101, 1, "=SUM(A1:A100)")
        })
        .unwrap();
        assert_eq!(book.value(0, 101, 1), Value::Number(5050.0));
    }

    #[test]
    fn csv_roundtrip() {
        let mut book = Book::new_empty("t").unwrap();
        book.paste_csv(0, CellRef { row: 1, col: 1 }, "a,b\n1,2\n").unwrap();
        assert_eq!(book.value(0, 2, 2), Value::Number(2.0));
        let csv = book.to_csv(0).unwrap();
        assert!(csv.starts_with("a,b\n"));
    }
}
