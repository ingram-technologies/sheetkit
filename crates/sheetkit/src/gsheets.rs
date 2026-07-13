//! Google Sheets pull/push as an *edge adapter*: the remote API is touched
//! exactly twice per session — one `spreadsheets.get` at open, one
//! `batchUpdate` at save. Everything in between runs against the local
//! engine, so remote latency, quotas and range limits never enter the
//! working loop.
//!
//! This module is pure: it converts API JSON to a [`Book`] plus a content
//! [`Baseline`], and a mutated `Book` plus that baseline back into
//! `batchUpdate` requests. Transport (HTTP, tokens) lives with the caller.
//!
//! Push is a *content diff* against the baseline taken at pull: cells whose
//! entered content (formula or literal) changed are written, cells that
//! disappeared are cleared, sheets that grew get grid resizes, new local
//! sheets are added. Conflict policy is last-write-wins; callers should
//! run a structural tripwire (compare current remote sheet properties with
//! [`Baseline::sheets`]) and surface a warning before pushing.

use std::collections::HashMap;

use serde_json::{json, Value as Json};

use crate::addr::{CellRef, Range};
use crate::book::{format_number, Book, Value};
use crate::{Error, Result};

/// Remote identity of one sheet, captured at pull time.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetMeta {
    pub sheet_id: i64,
    pub title: String,
    pub row_count: i32,
    pub column_count: i32,
}

/// What the spreadsheet looked like when it was pulled: enough to diff
/// against and to address the remote in a push.
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    pub spreadsheet_id: String,
    pub title: String,
    pub sheets: Vec<SheetMeta>,
    /// Entered content by `(sheet title, row, col)` — formulas and literals
    /// exactly as [`Book::content`] renders them.
    pub contents: HashMap<(String, i32, i32), String>,
}

pub struct Import {
    pub book: Book,
    pub baseline: Baseline,
    pub warnings: Vec<String>,
}

/// Extract a spreadsheet id from a docs.google.com URL, a `gsheets:<id>`
/// ref, or a bare id (only when it cannot be a file path).
pub fn parse_spreadsheet_id(input: &str) -> Option<String> {
    if let Some(rest) = input.strip_prefix("gsheets:") {
        return Some(rest.split(['/', '?', '#']).next()?.to_string()).filter(|s| !s.is_empty());
    }
    if input.contains("docs.google.com") {
        let marker = "/spreadsheets/d/";
        let at = input.find(marker)? + marker.len();
        let id: String = input[at..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        return Some(id).filter(|s| !s.is_empty());
    }
    None
}

// ---- pull -------------------------------------------------------------------

/// Build a workbook from a `spreadsheets.get` response (with grid data).
pub fn import(spreadsheet_id: &str, response: &Json) -> Result<Import> {
    let title = response
        .pointer("/properties/title")
        .and_then(Json::as_str)
        .unwrap_or("spreadsheet");
    let sheets = response
        .get("sheets")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::from("response has no sheets[] — was includeGridData requested?"))?;
    if sheets.is_empty() {
        return Err(Error::from("spreadsheet has no sheets"));
    }

    let mut book = Book::new_empty(title)?;
    let mut baseline = Baseline {
        spreadsheet_id: spreadsheet_id.to_string(),
        title: title.to_string(),
        ..Default::default()
    };
    let mut warnings: Vec<String> = Vec::new();
    let mut error_cells = 0usize;

    for (idx, sheet) in sheets.iter().enumerate() {
        let props = sheet.get("properties").cloned().unwrap_or(json!({}));
        let sheet_title = props
            .get("title")
            .and_then(Json::as_str)
            .unwrap_or("Sheet")
            .to_string();
        baseline.sheets.push(SheetMeta {
            sheet_id: props.get("sheetId").and_then(Json::as_i64).unwrap_or(0),
            title: sheet_title.clone(),
            row_count: props
                .pointer("/gridProperties/rowCount")
                .and_then(Json::as_i64)
                .unwrap_or(1000) as i32,
            column_count: props
                .pointer("/gridProperties/columnCount")
                .and_then(Json::as_i64)
                .unwrap_or(26) as i32,
        });

        let sheet_index = if idx == 0 {
            book.rename_sheet(0, &sheet_title)?;
            0
        } else {
            book.add_sheet(&sheet_title)?;
            idx as u32
        };

        // (row, col, input, date_fmt) collected first, applied in one batch.
        let mut inputs: Vec<(i32, i32, String)> = Vec::new();
        let mut date_fmts: Vec<(i32, i32, String)> = Vec::new();
        for grid in sheet.get("data").and_then(Json::as_array).unwrap_or(&vec![]) {
            let start_row = grid.get("startRow").and_then(Json::as_i64).unwrap_or(0) as i32;
            let start_col = grid.get("startColumn").and_then(Json::as_i64).unwrap_or(0) as i32;
            let row_data = grid.get("rowData").and_then(Json::as_array).cloned().unwrap_or_default();
            for (r, row) in row_data.iter().enumerate() {
                let values = row.get("values").and_then(Json::as_array).cloned().unwrap_or_default();
                for (c, cell) in values.iter().enumerate() {
                    let Some(entered) = cell.get("userEnteredValue") else {
                        continue;
                    };
                    let row_1 = start_row + r as i32 + 1;
                    let col_1 = start_col + c as i32 + 1;
                    let fmt_type = cell
                        .pointer("/effectiveFormat/numberFormat/type")
                        .and_then(Json::as_str)
                        .unwrap_or("");
                    if let Some(input) = entered_to_input(entered, fmt_type, &mut error_cells) {
                        if matches!(fmt_type, "DATE" | "DATE_TIME" | "TIME") {
                            let pattern = cell
                                .pointer("/effectiveFormat/numberFormat/pattern")
                                .and_then(Json::as_str)
                                .filter(|p| !p.is_empty())
                                .map(String::from)
                                .unwrap_or_else(|| default_date_pattern(fmt_type));
                            date_fmts.push((row_1, col_1, pattern));
                        }
                        inputs.push((row_1, col_1, input));
                    }
                }
            }
        }

        book.batch(|b| {
            for (row, col, input) in &inputs {
                b.set_input(sheet_index, *row, *col, input)?;
            }
            Ok(())
        })?;
        for (row, col, pattern) in date_fmts {
            book.set_num_fmt(sheet_index, Range::cell(CellRef { row, col }), &pattern)?;
        }
    }

    if error_cells > 0 {
        warnings.push(format!(
            "{error_cells} cell{} held error values remotely; imported as text literals",
            if error_cells == 1 { "" } else { "s" }
        ));
    }
    // Formulas the local engine cannot evaluate still round-trip as text,
    // but their computed values here are wrong — say so.
    let unsupported = count_unsupported(&book);
    if unsupported > 0 {
        warnings.push(format!(
            "{unsupported} formula{} not supported by the local engine (#N/IMPL!/#NAME?); they keep their original text and push back unchanged unless edited",
            if unsupported == 1 { " is" } else { "s are" }
        ));
    }

    baseline.contents = snapshot_contents(&book);
    Ok(Import { book, baseline, warnings })
}

/// Convert an ExtendedValue into engine input text.
fn entered_to_input(entered: &Json, fmt_type: &str, error_cells: &mut usize) -> Option<String> {
    if let Some(f) = entered.get("formulaValue").and_then(Json::as_str) {
        return Some(f.to_string());
    }
    if let Some(n) = entered.get("numberValue").and_then(Json::as_f64) {
        // Date serials transfer as-is: both worlds use the same 1899-12-30
        // epoch for modern dates; the pattern applied afterwards makes them
        // render as dates.
        let _ = fmt_type;
        return Some(format_number(n));
    }
    if let Some(s) = entered.get("stringValue").and_then(Json::as_str) {
        return Some(escape_literal(s));
    }
    if let Some(b) = entered.get("boolValue").and_then(Json::as_bool) {
        return Some(if b { "TRUE" } else { "FALSE" }.to_string());
    }
    if let Some(e) = entered.pointer("/errorValue/type").and_then(Json::as_str) {
        *error_cells += 1;
        return Some(escape_literal(&format!("#{e}")));
    }
    None
}

/// Text that would parse as something else needs the quote prefix.
fn escape_literal(s: &str) -> String {
    let parses_away = s.parse::<f64>().is_ok()
        || s.eq_ignore_ascii_case("true")
        || s.eq_ignore_ascii_case("false")
        || s.starts_with('=')
        || s.starts_with('\'');
    if parses_away {
        format!("'{s}")
    } else {
        s.to_string()
    }
}

fn default_date_pattern(fmt_type: &str) -> String {
    match fmt_type {
        "DATE_TIME" => "yyyy-mm-dd hh:mm".to_string(),
        "TIME" => "hh:mm:ss".to_string(),
        _ => "yyyy-mm-dd".to_string(),
    }
}

fn count_unsupported(book: &Book) -> usize {
    let mut n = 0;
    for sheet in 0..book.sheet_count() {
        book.for_each_cell(sheet, |row, col, _| {
            if let Value::Error(e) = book.value(sheet, row, col) {
                if e == "#N/IMPL!" || e == "#NAME?" {
                    n += 1;
                }
            }
        });
    }
    n
}

/// Entered content of every non-empty cell, keyed by `(sheet title, row, col)`.
pub fn snapshot_contents(book: &Book) -> HashMap<(String, i32, i32), String> {
    let mut map = HashMap::new();
    let names = book.sheet_names();
    for (idx, name) in names.iter().enumerate() {
        let sheet = idx as u32;
        let mut cells: Vec<(i32, i32)> = Vec::new();
        book.for_each_cell(sheet, |row, col, _| cells.push((row, col)));
        for (row, col) in cells {
            if let Ok(content) = book.content(sheet, row, col) {
                if !content.is_empty() {
                    map.insert((name.clone(), row, col), content);
                }
            }
        }
    }
    map
}

// ---- push -------------------------------------------------------------------

pub struct Push {
    /// `batchUpdate` requests, in application order.
    pub requests: Vec<Json>,
    pub changed_cells: usize,
    pub warnings: Vec<String>,
}

/// One changed cell, classified for the wire.
enum CellWrite {
    Set { value: Json, date_fmt: Option<String> },
    Clear,
}

/// Diff the book against the baseline and emit `batchUpdate` requests.
pub fn push_requests(book: &Book, baseline: &Baseline) -> Result<Push> {
    let mut requests: Vec<Json> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let current = snapshot_contents(book);
    let local_sheets = book.sheet_names();

    // Remote sheets that vanished locally are left alone, loudly.
    for meta in &baseline.sheets {
        if !local_sheets.iter().any(|n| n.eq_ignore_ascii_case(&meta.title)) {
            warnings.push(format!(
                "sheet {:?} exists remotely but not locally; it was NOT deleted remotely (delete it by hand if intended)",
                meta.title
            ));
        }
    }

    // Sheet id allocation for new local sheets.
    let max_remote_id = baseline.sheets.iter().map(|s| s.sheet_id).max().unwrap_or(0);
    let mut next_new_id = max_remote_id + 1001;

    let mut changed_cells = 0usize;
    for (idx, sheet_name) in local_sheets.iter().enumerate() {
        let sheet = idx as u32;
        let meta = baseline
            .sheets
            .iter()
            .find(|m| m.title.eq_ignore_ascii_case(sheet_name));
        let used = book.used_range(sheet);

        // Collect this sheet's writes: changed/new cells and cleared cells.
        let mut writes: HashMap<(i32, i32), CellWrite> = HashMap::new();
        let mut cells: Vec<(i32, i32)> = Vec::new();
        book.for_each_cell(sheet, |row, col, _| cells.push((row, col)));
        for (row, col) in cells {
            let content = book.content(sheet, row, col).unwrap_or_default();
            if content.is_empty() {
                continue;
            }
            let old = baseline.contents.get(&(sheet_name.clone(), row, col));
            if old.map(String::as_str) != Some(content.as_str()) {
                writes.insert((row, col), cell_write(book, sheet, row, col, &content));
            }
        }
        for (s, row, col) in baseline.contents.keys() {
            if s.eq_ignore_ascii_case(sheet_name)
                && !current.contains_key(&(s.clone(), *row, *col))
            {
                writes.insert((*row, *col), CellWrite::Clear);
            }
        }
        if writes.is_empty() && meta.is_some() {
            continue;
        }
        changed_cells += writes.len();

        let sheet_id = match meta {
            Some(m) => {
                // Grow the remote grid when local data outran it.
                if let Some(used) = used {
                    if used.end.row > m.row_count || used.end.col > m.column_count {
                        requests.push(json!({
                            "updateSheetProperties": {
                                "properties": {
                                    "sheetId": m.sheet_id,
                                    "gridProperties": {
                                        "rowCount": used.end.row.max(m.row_count),
                                        "columnCount": used.end.col.max(m.column_count),
                                    }
                                },
                                "fields": "gridProperties(rowCount,columnCount)"
                            }
                        }));
                    }
                }
                m.sheet_id
            }
            None => {
                let id = next_new_id;
                next_new_id += 1;
                let (rows, cols) = used
                    .map(|u| (u.end.row.max(100), u.end.col.max(26)))
                    .unwrap_or((100, 26));
                requests.push(json!({
                    "addSheet": {
                        "properties": {
                            "sheetId": id,
                            "title": sheet_name,
                            "gridProperties": { "rowCount": rows, "columnCount": cols }
                        }
                    }
                }));
                id
            }
        };

        requests.extend(writes_to_requests(sheet_id, writes));
    }

    Ok(Push { requests, changed_cells, warnings })
}

fn cell_write(book: &Book, sheet: u32, row: i32, col: i32, content: &str) -> CellWrite {
    if content.starts_with('=') {
        return CellWrite::Set {
            value: json!({ "formulaValue": content }),
            date_fmt: None,
        };
    }
    match book.value(sheet, row, col) {
        Value::Number(n) => {
            let fmt = book.num_fmt(sheet, row, col);
            let lower = fmt.to_lowercase();
            let is_date = lower != "general"
                && (lower.contains('y') || (lower.contains('d') && lower.contains('m')));
            CellWrite::Set {
                value: json!({ "numberValue": n }),
                date_fmt: is_date.then_some(fmt),
            }
        }
        Value::Bool(b) => CellWrite::Set { value: json!({ "boolValue": b }), date_fmt: None },
        Value::Error(e) => CellWrite::Set { value: json!({ "stringValue": e }), date_fmt: None },
        Value::Text(_) | Value::Empty => CellWrite::Set {
            // The raw text, not Value::Text — content strips the quote prefix.
            value: json!({ "stringValue": content.strip_prefix('\'').unwrap_or(content) }),
            date_fmt: None,
        },
    }
}

/// Group writes into per-row runs of consecutive columns, one `updateCells`
/// request per run. Runs carrying date formats use a wider field mask so the
/// remote cell renders as a date too.
fn writes_to_requests(sheet_id: i64, writes: HashMap<(i32, i32), CellWrite>) -> Vec<Json> {
    let mut keys: Vec<(i32, i32)> = writes.keys().copied().collect();
    keys.sort_unstable();

    let mut requests = Vec::new();
    let mut i = 0;
    while i < keys.len() {
        let (row, start_col) = keys[i];
        let run_has_fmt = |k: &(i32, i32)| {
            matches!(writes.get(k), Some(CellWrite::Set { date_fmt: Some(_), .. }))
        };
        let with_fmt = run_has_fmt(&keys[i]);
        let mut j = i;
        while j + 1 < keys.len()
            && keys[j + 1].0 == row
            && keys[j + 1].1 == keys[j].1 + 1
            && run_has_fmt(&keys[j + 1]) == with_fmt
        {
            j += 1;
        }
        let values: Vec<Json> = (i..=j)
            .map(|k| match &writes[&keys[k]] {
                CellWrite::Clear => json!({}),
                CellWrite::Set { value, date_fmt } => match date_fmt {
                    Some(fmt) => json!({
                        "userEnteredValue": value,
                        "userEnteredFormat": {
                            "numberFormat": { "type": "DATE", "pattern": fmt }
                        }
                    }),
                    None => json!({ "userEnteredValue": value }),
                },
            })
            .collect();
        let fields = if with_fmt {
            "userEnteredValue,userEnteredFormat.numberFormat"
        } else {
            "userEnteredValue"
        };
        requests.push(json!({
            "updateCells": {
                "start": {
                    "sheetId": sheet_id,
                    "rowIndex": row - 1,
                    "columnIndex": start_col - 1,
                },
                "fields": fields,
                "rows": [ { "values": values } ],
            }
        }));
        i = j + 1;
    }
    requests
}

/// After a successful push, fold the pushed changes back into the baseline
/// so the next save diffs incrementally: contents from the live book, sheet
/// metadata from the requests we just applied (added sheets carry
/// client-chosen ids; grid resizes update the counts).
pub fn apply_push_to_baseline(baseline: &mut Baseline, requests: &[Json], book: &Book) {
    for req in requests {
        if let Some(props) = req.pointer("/addSheet/properties") {
            baseline.sheets.push(SheetMeta {
                sheet_id: props.get("sheetId").and_then(Json::as_i64).unwrap_or(0),
                title: props
                    .get("title")
                    .and_then(Json::as_str)
                    .unwrap_or("Sheet")
                    .to_string(),
                row_count: props
                    .pointer("/gridProperties/rowCount")
                    .and_then(Json::as_i64)
                    .unwrap_or(100) as i32,
                column_count: props
                    .pointer("/gridProperties/columnCount")
                    .and_then(Json::as_i64)
                    .unwrap_or(26) as i32,
            });
        }
        if let Some(props) = req.pointer("/updateSheetProperties/properties") {
            let id = props.get("sheetId").and_then(Json::as_i64).unwrap_or(-1);
            if let Some(meta) = baseline.sheets.iter_mut().find(|m| m.sheet_id == id) {
                if let Some(r) = props.pointer("/gridProperties/rowCount").and_then(Json::as_i64) {
                    meta.row_count = r as i32;
                }
                if let Some(c) = props.pointer("/gridProperties/columnCount").and_then(Json::as_i64) {
                    meta.column_count = c as i32;
                }
            }
        }
    }
    baseline.contents = snapshot_contents(book);
}

/// Compare current remote sheet properties (a light `spreadsheets.get`
/// without grid data) against the pull-time baseline. Returns a warning when
/// the remote changed structurally underneath us.
pub fn structural_drift(baseline: &Baseline, remote_props: &Json) -> Option<String> {
    let remote: Vec<(i64, String)> = remote_props
        .get("sheets")
        .and_then(Json::as_array)
        .map(|sheets| {
            sheets
                .iter()
                .filter_map(|s| {
                    let p = s.get("properties")?;
                    Some((
                        p.get("sheetId").and_then(Json::as_i64)?,
                        p.get("title").and_then(Json::as_str)?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let ours: Vec<(i64, String)> = baseline
        .sheets
        .iter()
        .map(|m| (m.sheet_id, m.title.clone()))
        .collect();
    if remote != ours {
        Some(format!(
            "remote sheet structure changed since pull (was {:?}, now {:?}); pushing anyway (last-write-wins)",
            ours.iter().map(|(_, t)| t).collect::<Vec<_>>(),
            remote.iter().map(|(_, t)| t).collect::<Vec<_>>(),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Json {
        json!({
            "spreadsheetId": "abc123",
            "properties": { "title": "Q3 Orders" },
            "sheets": [
                {
                    "properties": {
                        "sheetId": 0,
                        "title": "Orders",
                        "gridProperties": { "rowCount": 1000, "columnCount": 26 }
                    },
                    "data": [ {
                        "rowData": [
                            { "values": [
                                { "userEnteredValue": { "stringValue": "Item" } },
                                { "userEnteredValue": { "stringValue": "Qty" } },
                                { "userEnteredValue": { "stringValue": "Date" } },
                                { "userEnteredValue": { "stringValue": "Total" } }
                            ]},
                            { "values": [
                                { "userEnteredValue": { "stringValue": "Ape" } },
                                { "userEnteredValue": { "numberValue": 2 } },
                                { "userEnteredValue": { "numberValue": 45292 },
                                  "effectiveFormat": { "numberFormat": { "type": "DATE", "pattern": "yyyy-mm-dd" } } },
                                { "userEnteredValue": { "formulaValue": "=B2*10" } }
                            ]},
                            { "values": [
                                { "userEnteredValue": { "stringValue": "007" } },
                                { "userEnteredValue": { "boolValue": true } },
                                {},
                                { "userEnteredValue": { "errorValue": { "type": "DIV_BY_ZERO" } } }
                            ]}
                        ]
                    } ]
                },
                {
                    "properties": {
                        "sheetId": 77,
                        "title": "Notes",
                        "gridProperties": { "rowCount": 100, "columnCount": 10 }
                    },
                    "data": [ {
                        "startRow": 4, "startColumn": 1,
                        "rowData": [ { "values": [
                            { "userEnteredValue": { "stringValue": "hello" } }
                        ]}]
                    } ]
                }
            ]
        })
    }

    #[test]
    fn parses_ids() {
        assert_eq!(
            parse_spreadsheet_id("https://docs.google.com/spreadsheets/d/1AbC-x_9/edit#gid=0"),
            Some("1AbC-x_9".to_string())
        );
        assert_eq!(parse_spreadsheet_id("gsheets:zzz9"), Some("zzz9".to_string()));
        assert_eq!(parse_spreadsheet_id("/tmp/books/file.xlsx"), None);
        assert_eq!(parse_spreadsheet_id("orders.csv"), None);
    }

    #[test]
    fn imports_fixture() {
        let imp = import("abc123", &fixture()).unwrap();
        let b = &imp.book;
        assert_eq!(b.sheet_names(), vec!["Orders", "Notes"]);
        assert_eq!(b.value(0, 2, 1), Value::Text("Ape".into()));
        assert_eq!(b.value(0, 2, 2), Value::Number(2.0));
        // Date serial renders as a date thanks to the applied pattern.
        assert_eq!(b.formatted_value(0, 2, 3), "2024-01-01");
        // Formula computes locally.
        assert_eq!(b.value(0, 2, 4), Value::Number(20.0));
        assert_eq!(b.formula(0, 2, 4).unwrap().as_deref(), Some("=B2*10"));
        // "007" stays text, TRUE is boolean, the error is a text literal.
        assert_eq!(b.value(0, 3, 1), Value::Text("007".into()));
        assert_eq!(b.value(0, 3, 2), Value::Bool(true));
        assert_eq!(b.value(0, 3, 4), Value::Text("#DIV_BY_ZERO".into()));
        // Offset grid data lands where startRow/startColumn say.
        assert_eq!(b.value(1, 5, 2), Value::Text("hello".into()));
        // Baseline captured.
        assert_eq!(imp.baseline.sheets.len(), 2);
        assert_eq!(imp.baseline.contents.get(&("Orders".into(), 2, 4)).unwrap(), "=B2*10");
        assert!(imp.warnings.iter().any(|w| w.contains("error value")), "{:?}", imp.warnings);
    }

    #[test]
    fn no_changes_pushes_nothing() {
        let imp = import("abc123", &fixture()).unwrap();
        let push = push_requests(&imp.book, &imp.baseline).unwrap();
        assert_eq!(push.changed_cells, 0, "{:?}", push.requests);
        assert!(push.requests.is_empty());
    }

    #[test]
    fn diff_produces_grouped_updates() {
        let mut imp = import("abc123", &fixture()).unwrap();
        let b = &mut imp.book;
        b.set_input(0, 2, 2, "5").unwrap(); // change Qty
        b.set_input(0, 4, 1, "Bee").unwrap(); // new row, two consecutive cells
        b.set_input(0, 4, 2, "3").unwrap();
        b.clear_contents(0, Range::cell(CellRef { row: 3, col: 1 })).unwrap(); // clear "007"

        let push = push_requests(b, &imp.baseline).unwrap();
        // B2 changed + A4:B4 new + A3 cleared + D2 recalc does NOT count
        // (=B2*10 content unchanged).
        assert_eq!(push.changed_cells, 4, "{:?}", push.requests);

        let updates: Vec<&Json> = push.requests.iter().filter(|r| r.get("updateCells").is_some()).collect();
        // A4:B4 grouped into one run.
        let run = updates
            .iter()
            .find(|r| r.pointer("/updateCells/start/rowIndex") == Some(&json!(3)))
            .expect("A4 run");
        assert_eq!(run.pointer("/updateCells/rows/0/values").unwrap().as_array().unwrap().len(), 2);
        // The clear is an empty CellData under a userEnteredValue mask.
        let clear = updates
            .iter()
            .find(|r| r.pointer("/updateCells/start/rowIndex") == Some(&json!(2)))
            .expect("clear run");
        assert_eq!(clear.pointer("/updateCells/rows/0/values/0").unwrap(), &json!({}));
        assert_eq!(
            clear.pointer("/updateCells/fields").unwrap(),
            &json!("userEnteredValue")
        );
    }

    #[test]
    fn new_sheet_and_grid_growth() {
        let mut imp = import("abc123", &fixture()).unwrap();
        let b = &mut imp.book;
        b.add_sheet("Summary").unwrap();
        b.set_input(2, 1, 1, "totals").unwrap();
        b.set_input(0, 1200, 1, "beyond the grid").unwrap(); // rowCount was 1000

        let push = push_requests(b, &imp.baseline).unwrap();
        let add = push
            .requests
            .iter()
            .find(|r| r.get("addSheet").is_some())
            .expect("addSheet request");
        assert_eq!(add.pointer("/addSheet/properties/title").unwrap(), &json!("Summary"));
        let resize = push
            .requests
            .iter()
            .find(|r| r.get("updateSheetProperties").is_some())
            .expect("grid resize");
        assert_eq!(
            resize.pointer("/updateSheetProperties/properties/gridProperties/rowCount").unwrap(),
            &json!(1200)
        );
    }

    #[test]
    fn date_cells_push_with_format() {
        let imp = import("abc123", &fixture()).unwrap();
        let mut book = imp.book;
        book.set_input(0, 5, 3, "2024-06-30").unwrap(); // engine assigns a date fmt

        let push = push_requests(&book, &imp.baseline).unwrap();
        let req = push
            .requests
            .iter()
            .find(|r| r.pointer("/updateCells/start/rowIndex") == Some(&json!(4)))
            .expect("date write");
        assert!(
            req.pointer("/updateCells/fields")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("numberFormat"),
            "{req}"
        );
        let v = req.pointer("/updateCells/rows/0/values/0/userEnteredValue/numberValue");
        assert!(v.is_some(), "{req}");
    }

    #[test]
    fn structural_drift_detected() {
        let imp = import("abc123", &fixture()).unwrap();
        let same = json!({ "sheets": [
            { "properties": { "sheetId": 0, "title": "Orders" } },
            { "properties": { "sheetId": 77, "title": "Notes" } }
        ]});
        assert!(structural_drift(&imp.baseline, &same).is_none());
        let changed = json!({ "sheets": [
            { "properties": { "sheetId": 0, "title": "Orders" } }
        ]});
        assert!(structural_drift(&imp.baseline, &changed).is_some());
    }
}
