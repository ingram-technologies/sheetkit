//! Text encodings of workbook state, rendered under an explicit token budget.
//!
//! Three shapes, chosen automatically by range size and structure:
//! - **dense** — row-anchored grid with formulas and computed values together
//! - **aggregated** — per-column facts for large homogeneous regions
//! - **sparse** — explicit `addr: value` lines for scattered cells
//!
//! Every truncation is announced. Silent elision is the one unforgivable sin
//! here: a model that cannot trust its view will re-read everything.

use crate::addr::{display_sheet, number_to_column, CellRef};
use crate::book::{Book, Resolved, Value};
use crate::regions::{Dtype, Region, Sortedness};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dense,
    Aggregated,
    Sparse,
}

#[derive(Debug, Clone, Copy)]
pub struct ViewOptions {
    pub mode: Option<Mode>,
    /// Approximate output budget in tokens (chars / 4).
    pub budget_tokens: usize,
}

impl Default for ViewOptions {
    fn default() -> Self {
        ViewOptions { mode: None, budget_tokens: 2000 }
    }
}

/// Cells at or under this count render dense by default.
const DENSE_CELL_LIMIT: i64 = 600;

pub fn render_view(book: &Book, target: &Resolved, regions: &[Region], opts: ViewOptions) -> String {
    let budget_chars = opts.budget_tokens.saturating_mul(4).max(400);
    let region = regions.iter().find(|r| {
        r.sheet == target.sheet_index
            && r.range.start.row <= target.range.start.row
            && r.range.end.row >= target.range.end.row
            && r.range.start.col <= target.range.start.col
            && r.range.end.col >= target.range.end.col
    });
    let mode = opts.mode.unwrap_or_else(|| {
        if target.range.cell_count() <= DENSE_CELL_LIMIT {
            Mode::Dense
        } else if region.is_some() {
            Mode::Aggregated
        } else {
            Mode::Dense // dense with head/tail elision
        }
    });
    match mode {
        Mode::Dense => render_dense(book, target, region, budget_chars),
        Mode::Sparse => render_sparse(book, target, budget_chars),
        Mode::Aggregated => match region {
            Some(r) => render_region(book, r),
            None => format!(
                "{} does not overlap a detected region; showing dense view instead.\n{}",
                target.qualified(),
                render_dense(book, target, None, budget_chars)
            ),
        },
    }
}

/// One cell for display: formula and value together where a formula exists.
fn cell_text(book: &Book, sheet: u32, row: i32, col: i32) -> String {
    let value = book.value(sheet, row, col);
    let formula = book.formula(sheet, row, col).ok().flatten();
    match (formula, &value) {
        (Some(f), v) => format!("{f} ⇒ {}", display_value(book, sheet, row, col, v)),
        (None, Value::Empty) => String::new(),
        (None, v) => display_value(book, sheet, row, col, v),
    }
}

/// Numbers formatted as dates/currency render through their number format.
fn display_value(book: &Book, sheet: u32, row: i32, col: i32, v: &Value) -> String {
    if let Value::Number(_) = v {
        let fmt = book.num_fmt(sheet, row, col);
        let lower = fmt.to_lowercase();
        if lower != "general" && (lower.contains('y') || (lower.contains('d') && lower.contains('m'))) {
            return book.formatted_value(sheet, row, col);
        }
    }
    v.display()
}

pub fn render_dense(
    book: &Book,
    target: &Resolved,
    region: Option<&Region>,
    budget_chars: usize,
) -> String {
    let sheet = target.sheet_index;
    let range = target.range;
    let mut header = format!("{}", target.qualified());
    if let Some(r) = region {
        header.push_str(&format!(" [region: {}]", r.name));
    }

    // Column header line: letters, plus header names when a region knows them.
    let cols: Vec<i32> = (range.start.col..=range.end.col).collect();
    let mut col_titles: Vec<String> = Vec::with_capacity(cols.len());
    for &col in &cols {
        let letter = number_to_column(col).unwrap_or_default();
        match region.and_then(|r| r.header_of(col)) {
            Some(h) => col_titles.push(format!("{letter}\"{h}\"")),
            None => col_titles.push(letter),
        }
    }

    // Build row cell texts, eliding from the middle if the budget runs out.
    let total_rows = range.height();
    let mut lines: Vec<(i32, Vec<String>)> = Vec::new();
    let mut used_chars = 0usize;
    let per_row_reserve = 16;
    let mut elided_from: Option<i32> = None;
    let tail_rows = 2.min(total_rows as usize);

    for row in range.start.row..=range.end.row {
        let cells: Vec<String> = cols.iter().map(|&c| cell_text(book, sheet, row, c)).collect();
        used_chars += cells.iter().map(|s| s.len() + 2).sum::<usize>() + per_row_reserve;
        lines.push((row, cells));
        if used_chars > budget_chars && (row as i64) < range.end.row as i64 - tail_rows as i64 {
            elided_from = Some(row + 1);
            break;
        }
    }
    if elided_from.is_some() {
        for row in (range.end.row - tail_rows as i32 + 1)..=range.end.row {
            let cells: Vec<String> = cols.iter().map(|&c| cell_text(book, sheet, row, c)).collect();
            lines.push((row, cells));
        }
    }

    // Skip long runs of fully-empty rows.
    let mut kept: Vec<RowLine> = Vec::new();
    let mut empty_run: Vec<i32> = Vec::new();
    for (row, cells) in lines {
        if cells.iter().all(|c| c.is_empty()) {
            empty_run.push(row);
        } else {
            flush_empty(&mut kept, &mut empty_run);
            kept.push(RowLine::Cells(row, cells));
        }
    }
    flush_empty(&mut kept, &mut empty_run);

    // Column widths from what we kept.
    let mut widths: Vec<usize> = col_titles.iter().map(|t| t.len()).collect();
    for line in &kept {
        if let RowLine::Cells(_, cells) = line {
            for (i, c) in cells.iter().enumerate() {
                widths[i] = widths[i].max(display_len(c));
            }
        }
    }
    for w in &mut widths {
        *w = (*w).min(40);
    }

    let row_digits = range.end.row.to_string().len();
    let mut out = header;
    out.push('\n');
    out.push_str(&format!("{:>row_digits$} | ", ""));
    for (i, t) in col_titles.iter().enumerate() {
        out.push_str(&pad(t, widths[i]));
        out.push_str("  ");
    }
    out.push('\n');
    for line in &kept {
        match line {
            RowLine::Cells(row, cells) => {
                if let Some(from) = elided_from {
                    if *row >= from && *row > range.end.row - tail_rows as i32 && !out.ends_with("⋮\n") {
                        // fallthrough; elision marker handled below
                    }
                }
                out.push_str(&format!("{row:>row_digits$} | "));
                for (i, c) in cells.iter().enumerate() {
                    out.push_str(&pad(&truncate(c, 40), widths[i]));
                    out.push_str("  ");
                }
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
            }
            RowLine::EmptyGap(from, to) => {
                out.push_str(&format!(
                    "{:>row_digits$} · rows {from}–{to} empty\n",
                    "⋮"
                ));
            }
        }
    }
    if let Some(from) = elided_from {
        let to = range.end.row - tail_rows as i32;
        if to >= from {
            let n = to - from + 1;
            out.push_str(&format!(
                "… {n} rows ({from}–{to}) elided for budget; `view {}!{}{}:{}{}` to expand or raise budget\n",
                display_sheet(&target.sheet_name),
                number_to_column(range.start.col).unwrap_or_default(),
                from,
                number_to_column(range.end.col).unwrap_or_default(),
                to,
            ));
        }
    }
    out.trim_end().to_string()
}

enum RowLine {
    Cells(i32, Vec<String>),
    EmptyGap(i32, i32),
}

fn flush_empty(kept: &mut Vec<RowLine>, run: &mut Vec<i32>) {
    match run.len() {
        0 => {}
        1..=2 => {
            for row in run.iter() {
                kept.push(RowLine::Cells(*row, vec![]));
            }
        }
        _ => kept.push(RowLine::EmptyGap(run[0], run[run.len() - 1])),
    }
    run.clear();
}

fn display_len(s: &str) -> usize {
    s.chars().count()
}

fn pad(s: &str, width: usize) -> String {
    let len = display_len(s);
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if display_len(s) <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

pub fn render_sparse(book: &Book, target: &Resolved, budget_chars: usize) -> String {
    let mut out = format!("{} (sparse)\n", target.qualified());
    let mut used = out.len();
    let mut shown = 0usize;
    let mut total = 0usize;
    let mut truncated = false;
    let mut entries: Vec<(i32, i32)> = Vec::new();
    book.for_each_cell(target.sheet_index, |row, col, _| {
        if target.range.contains(row, col) {
            entries.push((row, col));
        }
    });
    entries.sort_unstable();
    for (row, col) in entries {
        total += 1;
        if truncated {
            continue;
        }
        let text = cell_text(book, target.sheet_index, row, col);
        if text.is_empty() {
            continue;
        }
        let line = format!("{}: {}\n", CellRef { row, col }.a1(), text);
        if used + line.len() > budget_chars {
            truncated = true;
            continue;
        }
        used += line.len();
        out.push_str(&line);
        shown += 1;
    }
    if truncated {
        out.push_str(&format!("… {} more cells elided for budget\n", total - shown));
    }
    if shown == 0 && !truncated {
        out.push_str("(no non-empty cells)\n");
    }
    out.trim_end().to_string()
}

/// The aggregated, per-column view of a region.
pub fn render_region(_book: &Book, region: &Region) -> String {
    let mut out = format!(
        "{} ({}!{}, {} row{}{})\n",
        region.name,
        display_sheet(&region.sheet_name),
        region.range.a1(),
        region.body_rows(),
        if region.body_rows() == 1 { "" } else { "s" },
        if region.header_row.is_some() { " + header" } else { "" },
    );
    for c in &region.columns {
        let letter = number_to_column(c.col).unwrap_or_default();
        let head = match &c.header {
            Some(h) => format!("{letter} {h:?}"),
            None => letter.clone(),
        };
        let mut facts: Vec<String> = Vec::new();
        // A dominant fill is the most informative fact — lead with it.
        if let Some(fill) = &c.fill {
            let mut f = format!("{} fill {}", fill.anchor_formula, fill.range().a1());
            if let (Some(mn), Some(mx)) = (&c.min_display, &c.max_display) {
                f.push_str(&format!(" ⇒ {mn}..{mx}"));
            }
            facts.push(f);
            if !fill.breaks.is_empty() {
                let shown: Vec<String> = fill
                    .breaks
                    .iter()
                    .take(5)
                    .map(|r| format!("{letter}{r}"))
                    .collect();
                let more = if fill.breaks.len() > 5 {
                    format!(" +{}", fill.breaks.len() - 5)
                } else {
                    String::new()
                };
                facts.push(format!("⚠ breaks fill: {}{more}", shown.join(",")));
            }
        } else {
            match c.dtype {
                Dtype::Empty => facts.push("empty".to_string()),
                Dtype::Number | Dtype::Date => {
                    let label = c.dtype.label();
                    match (&c.min_display, &c.max_display) {
                        (Some(mn), Some(mx)) if mn == mx => facts.push(format!("{label} constant {mn}")),
                        (Some(mn), Some(mx)) => facts.push(format!("{label} {mn}..{mx}")),
                        _ => facts.push(label.to_string()),
                    }
                }
                Dtype::Text | Dtype::Bool | Dtype::Mixed => {
                    let mut f = c.dtype.label().to_string();
                    if let Some(d) = c.distinct {
                        f.push_str(&format!(", {d} distinct"));
                    }
                    facts.push(f);
                }
            }
            match c.sorted {
                Sortedness::Asc => facts.push("sorted asc".to_string()),
                Sortedness::Desc => facts.push("sorted desc".to_string()),
                _ => {}
            }
            if !c.top_values.is_empty() && c.top_values.len() <= 4 && c.dtype != Dtype::Number {
                let tops: Vec<String> = c
                    .top_values
                    .iter()
                    .map(|(v, n)| if *n > 1 { format!("{v}×{n}") } else { v.clone() })
                    .collect();
                facts.push(format!("values: {}", tops.join(", ")));
            }
        }
        if c.non_empty < c.body_rows {
            facts.push(format!("{} empty", c.body_rows - c.non_empty));
        }
        if c.error_count > 0 {
            facts.push(format!("⚠ {} error{}", c.error_count, if c.error_count == 1 { "" } else { "s" }));
        }
        out.push_str(&format!("  {}  {}\n", pad(&head, 12), facts.join(" · ")));
    }
    out.trim_end().to_string()
}

/// The workbook sketch: what `sheet_open` returns. Every sheet, every region,
/// aggregated views throughout, defined names — the whole shape at a glance.
pub fn sketch(book: &Book, regions: &[Region]) -> String {
    let names = book.sheet_names();
    let mut out = format!(
        "workbook {:?} — {} sheet{}\n",
        book.name(),
        names.len(),
        if names.len() == 1 { "" } else { "s" }
    );
    for (i, name) in names.iter().enumerate() {
        let sheet = i as u32;
        match book.used_range(sheet) {
            None => out.push_str(&format!("{}: empty\n", display_sheet(name))),
            Some(used) => {
                out.push_str(&format!("{}: used {}\n", display_sheet(name), used.a1()));
                for r in regions.iter().filter(|r| r.sheet == sheet) {
                    let rendered = render_region(book, r);
                    for line in rendered.lines() {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
    }
    let defined = book.defined_names();
    if !defined.is_empty() {
        let list: Vec<String> = defined
            .iter()
            .map(|(n, _s, f)| format!("{n} = {f}"))
            .collect();
        out.push_str(&format!("defined names: {}\n", list.join(", ")));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regions::detect_all;

    fn sample_book() -> Book {
        let mut book = Book::new_empty("demo").unwrap();
        book.batch(|b| {
            b.set_input(0, 1, 1, "Item")?;
            b.set_input(0, 1, 2, "Qty")?;
            b.set_input(0, 1, 3, "Price")?;
            b.set_input(0, 1, 4, "Total")?;
            let rows = [("Ape", 2, 3.5), ("Bee", 10, 0.4), ("Cat", 1, 12.0)];
            for (i, (item, qty, price)) in rows.iter().enumerate() {
                let row = i as i32 + 2;
                b.set_input(0, row, 1, item)?;
                b.set_input(0, row, 2, &qty.to_string())?;
                b.set_input(0, row, 3, &price.to_string())?;
                b.set_input(0, row, 4, &format!("=B{row}*C{row}"))?;
            }
            b.set_input(0, 5, 4, "=SUM(D2:D4)")?;
            Ok(())
        })
        .unwrap();
        book
    }

    fn resolved(book: &Book, target: &str) -> Resolved {
        let t = crate::addr::parse_target(target).unwrap();
        book.resolve(&t, 0, &Default::default()).unwrap()
    }

    #[test]
    fn dense_shows_formulas_and_values() {
        let book = sample_book();
        let (regions, _) = detect_all(&book);
        let v = render_view(&book, &resolved(&book, "A1:D5"), &regions, ViewOptions::default());
        assert!(v.contains("=B2*C2 ⇒ 7"), "{v}");
        assert!(v.contains("\"Ape\""), "{v}");
        assert!(v.contains("A\"Item\""), "{v}");
        assert!(v.contains("=SUM(D2:D4) ⇒ 23"), "{v}");
    }

    #[test]
    fn budget_elides_with_announcement() {
        let mut book = Book::new_empty("big").unwrap();
        book.batch(|b| {
            for row in 1..=300 {
                b.set_input(0, row, 1, &format!("value number {row}"))?;
                b.set_input(0, row, 2, &format!("{row}"))?;
            }
            Ok(())
        })
        .unwrap();
        let (regions, _) = detect_all(&book);
        let v = render_view(
            &book,
            &resolved(&book, "A1:B300"),
            &regions,
            ViewOptions { mode: Some(Mode::Dense), budget_tokens: 300 },
        );
        assert!(v.contains("elided for budget"), "{v}");
        assert!(v.contains("300 |"), "tail row shown: {v}");
    }

    #[test]
    fn sketch_covers_workbook() {
        let book = sample_book();
        let (regions, _) = detect_all(&book);
        let s = sketch(&book, &regions);
        assert!(s.contains("workbook \"demo\""), "{s}");
        assert!(s.contains("table1"), "{s}");
        assert!(s.contains("=B2*C2 fill"), "{s}");
    }

    #[test]
    fn sparse_lists_cells() {
        let mut book = Book::new_empty("s").unwrap();
        book.set_input(0, 1, 1, "9").unwrap();
        book.set_input(0, 50, 3, "hello").unwrap();
        let (regions, _) = detect_all(&book);
        let v = render_view(
            &book,
            &resolved(&book, "A1:E60"),
            &regions,
            ViewOptions { mode: Some(Mode::Sparse), budget_tokens: 500 },
        );
        assert!(v.contains("A1: 9"), "{v}");
        assert!(v.contains("C50: \"hello\""), "{v}");
    }
}
