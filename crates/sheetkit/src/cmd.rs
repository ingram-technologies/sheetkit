//! The command layer: a line-oriented DSL shared by the MCP `sheet_exec`
//! tool, the REPL, and any embedding caller.
//!
//! A script is a sequence of lines; each line is one command. Blank lines and
//! `#` comments are skipped. Execution stops at the first failing line and the
//! partial state is reported honestly. Every run ends with a recalc delta —
//! exactly which cells changed value, old ⇒ new.
//!
//! ```text
//! view orders                       # or: view Sheet1!A1:F30 mode=dense budget=500
//! set D2 =B2*C2
//! set B2:B4 = [2, 10, 1]
//! fill D2 -> D2:D9871
//! clear C5:C10 formats
//! insert rows 5 count 3
//! delete cols D:E in Sheet2
//! sort orders by Total desc, Item asc
//! find "Berlin" in Sheet1 values
//! name total = Sheet1!F9871
//! sheet new "Q3"
//! checkpoint before-cleanup
//! undo | redo | restore before-cleanup
//! expect F9871 == 1204.5
//! highlight D455 color=amber note="breaks the fill — intended?"
//! ```

use std::collections::HashMap;

use crate::addr::{display_sheet, number_to_column, CellRef, Range};
use crate::book::{Resolved, Value};
use crate::delta::{self, Delta};
use crate::displace::displace_rows;
use crate::session::Session;
use crate::view::{self, Mode, ViewOptions};
use crate::{Error, Result};

/// Everything a script run produced.
#[derive(Debug, Default)]
pub struct ExecOutput {
    /// One entry per executed command (its printable result).
    pub results: Vec<String>,
    pub delta: Delta,
    pub warnings: Vec<String>,
    /// `(line number, command, error)` when a line failed; later lines did not run.
    pub failed: Option<(usize, String, String)>,
    /// The book was swapped wholesale (restore / journal undo): the engine
    /// diff queue cannot represent this exec, replicas must resync.
    pub needs_resync: bool,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.failed.is_none()
    }

    /// Render for a model/human: command results, then the recalc echo,
    /// then warnings and the failure if any.
    pub fn render(&self, multi_sheet: bool) -> String {
        let mut out = String::new();
        for r in &self.results {
            if !r.is_empty() {
                out.push_str(r);
                out.push('\n');
            }
        }
        out.push_str(&delta::render(&self.delta, multi_sheet, 20));
        out.push('\n');
        for w in &self.warnings {
            out.push_str(&format!("⚠ {w}\n"));
        }
        if let Some((line, cmd, err)) = &self.failed {
            out.push_str(&format!(
                "✗ line {line} `{cmd}` failed: {err}\n  (script stopped there; earlier lines ARE applied — see recalc above)\n"
            ));
        }
        out.trim_end().to_string()
    }
}

/// Execute a DSL script against a session. `author` labels highlights.
pub fn exec(session: &mut Session, script: &str, author: &str) -> ExecOutput {
    let mut out = ExecOutput::default();
    session.needs_resync = false;
    let before = delta::snapshot(&session.book);

    for (i, raw_line) in script.lines().enumerate() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        match run_line(session, &line, author, &mut out.warnings) {
            Ok(result) => out.results.push(result),
            Err(e) => {
                out.failed = Some((i + 1, line, e.0));
                break;
            }
        }
    }

    out.delta = delta::diff(&before, &session.book);
    out.needs_resync = session.needs_resync;
    if !out.delta.is_empty() {
        session.invalidate();
    }
    out
}

fn strip_comment(line: &str) -> &str {
    // A `#` outside quotes starts a comment.
    let mut in_dq = false;
    let mut in_sq = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' if !in_sq => in_dq = !in_dq,
            '\'' if !in_dq => in_sq = !in_sq,
            '#' if !in_dq && !in_sq => return &line[..i],
            _ => {}
        }
    }
    line
}

fn run_line(
    session: &mut Session,
    line: &str,
    author: &str,
    warnings: &mut Vec<String>,
) -> Result<String> {
    let (verb, rest) = split_word(line);
    let verb_lc = verb.to_ascii_lowercase();
    match verb_lc.as_str() {
        "sketch" => cmd_sketch(session),
        "sheets" => cmd_sheets(session),
        "regions" => cmd_regions(session),
        "view" => cmd_view(session, rest),
        "set" => cmd_set(session, rest),
        "fill" => cmd_fill(session, rest),
        "clear" => cmd_clear(session, rest),
        "insert" | "delete" => cmd_structure(session, &verb_lc, rest),
        "sort" => cmd_sort(session, rest, warnings),
        "find" => cmd_find(session, rest),
        "name" => cmd_name(session, rest),
        "unname" => cmd_unname(session, rest),
        "names" => cmd_names(session),
        "sheet" => cmd_sheet(session, rest),
        "checkpoint" => cmd_checkpoint(session, rest),
        "checkpoints" => Ok(format!(
            "checkpoints: {}",
            non_empty_or(session.checkpoint_names().join(", "), "(none)")
        )),
        "restore" => cmd_restore(session, rest),
        "undo" => cmd_undo(session),
        "redo" => cmd_redo(session),
        "expect" => cmd_expect(session, rest),
        "highlight" => cmd_highlight(session, rest, author),
        "unhighlight" => cmd_unhighlight(session, rest),
        "highlights" => cmd_highlights(session),
        "help" => Ok(HELP.trim().to_string()),
        _ => Err(Error::from(format!(
            "unknown command {verb:?} — try `help` for the command list"
        ))),
    }
}

pub const HELP: &str = r#"
commands (one per line, # comments):
  sketch · sheets · regions · names · checkpoints · highlights · help
  view <target> [mode=dense|agg|sparse] [budget=<tokens>]
  set <cell> <value or =formula>          set <range> = [v1, v2, …]
  set <range> <value or =formula>         (formula re-anchors per row)
  fill <source> -> <target-range>
  clear <target> [contents|formats|all]
  insert rows <at> [count <n>] [in <sheet>]     insert cols <C> [count <n>] [in <sheet>]
  delete rows <a>[:<b>] [in <sheet>]            delete cols <C>[:<E>] [in <sheet>]
  sort <target> by <column or "Header"> [asc|desc][, …]
  find "<text>" [in <sheet or range>] [values|formulas]
  name <ident> = <range> · unname <ident>
  sheet new|select|delete "<name>" · sheet rename "<old>" -> "<new>"
  checkpoint <name> · restore <name> · undo · redo
  expect <cell> <==|!=|>|>=|<|<=> <value>
  highlight <range> [color=<c>] [note="…"] · unhighlight <id>
targets: A1 · A1:C10 · C:E · 5:20 · Sheet2!B3 · 'My Sheet'!A1 · region or defined name
"#;

// ---- helpers ---------------------------------------------------------------

fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn non_empty_or(s: String, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

/// Tokenize respecting `"…"` and `'…'` quoting. Quotes are kept in the token
/// so callers can tell quoted strings from bare words.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Resolve, requiring a single cell.
fn resolve_cell(session: &mut Session, target: &str) -> Result<(Resolved, CellRef)> {
    let r = session.resolve(target)?;
    if !r.range.is_single_cell() {
        return Err(Error::from(format!(
            "{target:?} is a range; this command needs a single cell"
        )));
    }
    let cell = r.range.start;
    Ok((r, cell))
}

/// Parse a trailing `in <sheet>` clause off the token list.
fn parse_in_sheet(session: &mut Session, tokens: &mut Vec<String>) -> Result<u32> {
    if tokens.len() >= 2 && tokens[tokens.len() - 2].eq_ignore_ascii_case("in") {
        let name = unquote(&tokens.pop().unwrap());
        tokens.pop();
        session
            .book
            .sheet_index(&name)
            .ok_or_else(|| Error::from(format!("no sheet named {name:?}")))
    } else {
        Ok(session.current_sheet)
    }
}

// ---- read commands ---------------------------------------------------------

fn cmd_sketch(session: &mut Session) -> Result<String> {
    let (regions, _) = session_regions(session);
    Ok(view::sketch(&session.book, &regions))
}

fn session_regions(
    session: &mut Session,
) -> (Vec<crate::regions::Region>, HashMap<String, (u32, Range)>) {
    let (r, i) = session.regions();
    (r.clone(), i.clone())
}

fn cmd_sheets(session: &mut Session) -> Result<String> {
    let names = session.book.sheet_names();
    let current = session.current_sheet as usize;
    let lines: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let used = session
                .book
                .used_range(i as u32)
                .map(|r| format!("used {}", r.a1()))
                .unwrap_or_else(|| "empty".to_string());
            format!(
                "{}{} — {used}",
                if i == current { "* " } else { "  " },
                display_sheet(n)
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn cmd_regions(session: &mut Session) -> Result<String> {
    let (regions, _) = session_regions(session);
    if regions.is_empty() {
        return Ok("no regions detected (workbook is empty)".to_string());
    }
    let lines: Vec<String> = regions
        .iter()
        .map(|r| {
            let headers: Vec<String> = r.columns.iter().filter_map(|c| c.header.clone()).collect();
            let head = if headers.is_empty() {
                String::new()
            } else {
                format!(" · headers: {}", headers.join(", "))
            };
            format!(
                "{} — {}!{} ({} row{}{}){head}",
                r.name,
                display_sheet(&r.sheet_name),
                r.range.a1(),
                r.body_rows(),
                if r.body_rows() == 1 { "" } else { "s" },
                if r.header_row.is_some() {
                    " + header"
                } else {
                    ""
                },
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn cmd_view(session: &mut Session, rest: &str) -> Result<String> {
    let mut tokens = tokenize(rest);
    if tokens.is_empty() {
        return Err(Error::from(
            "usage: view <target> [mode=dense|agg|sparse] [budget=<tokens>]",
        ));
    }
    let mut opts = ViewOptions::default();
    let mut target_parts: Vec<String> = Vec::new();
    for t in tokens.drain(..) {
        if let Some(v) = t.strip_prefix("mode=") {
            opts.mode = Some(match v {
                "dense" => Mode::Dense,
                "agg" | "aggregated" => Mode::Aggregated,
                "sparse" => Mode::Sparse,
                other => return Err(Error::from(format!("unknown mode {other:?}"))),
            });
        } else if let Some(v) = t.strip_prefix("budget=") {
            opts.budget_tokens = v
                .parse()
                .map_err(|_| Error::from(format!("bad budget {v:?}")))?;
        } else {
            target_parts.push(t);
        }
    }
    let target = target_parts.join(" ");
    let resolved = session.resolve(&target)?;
    // Track the sheet for later unqualified references.
    session.current_sheet = resolved.sheet_index;
    let (regions, _) = session_regions(session);
    Ok(view::render_view(&session.book, &resolved, &regions, opts))
}

// ---- write commands ----------------------------------------------------------

fn cmd_set(session: &mut Session, rest: &str) -> Result<String> {
    let (target_str, raw) = split_word(rest);
    if target_str.is_empty() || raw.is_empty() {
        return Err(Error::from(
            "usage: set <cell|range> <value | =formula | = [v1, v2, …]>",
        ));
    }
    let resolved = session.resolve(target_str)?;
    let sheet = resolved.sheet_index;
    let range = resolved.range;

    // Batch list form: `set B2:B4 = [2, 10, 1]`
    let trimmed = raw.trim();
    let list = trimmed
        .strip_prefix('=')
        .map(str::trim)
        .filter(|t| t.starts_with('['))
        .or_else(|| Some(trimmed).filter(|t| t.starts_with('[')));
    if let Some(list) = list {
        let inner = list
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| Error::from("list must be [a, b, c]"))?;
        let values: Vec<String> = split_list(inner).iter().map(|v| unquote(v)).collect();
        let count = range.cell_count();
        if values.len() as i64 != count {
            return Err(Error::from(format!(
                "list has {} values but {} has {count} cells",
                values.len(),
                range.a1()
            )));
        }
        let mut iter = values.into_iter();
        session.book.batch(|b| {
            for row in range.start.row..=range.end.row {
                for col in range.start.col..=range.end.col {
                    let v = iter.next().unwrap();
                    if v.is_empty() {
                        b.clear_contents(sheet, Range::cell(CellRef { row, col }))?;
                    } else {
                        b.set_input(sheet, row, col, &v)?;
                    }
                }
            }
            Ok(())
        })?;
        session.invalidate();
        return Ok(format!("set {} ({count} cells)", range.a1()));
    }

    let value = unquote(raw);
    if range.is_single_cell() {
        session
            .book
            .set_input(sheet, range.start.row, range.start.col, &value)?;
        session.invalidate();
        return Ok(format!("set {}", resolved.qualified()));
    }

    // Multi-cell: formulas re-anchor per row via displacement; literals repeat.
    session.book.batch(|b| {
        for row in range.start.row..=range.end.row {
            for col in range.start.col..=range.end.col {
                let v = if value.starts_with('=') {
                    displace_rows(&value, row - range.start.row)
                } else {
                    value.clone()
                };
                b.set_input(sheet, row, col, &v)?;
            }
        }
        Ok(())
    })?;
    session.invalidate();
    let note = if value.starts_with('=') && range.height() > 1 {
        " (formula re-anchored per row)"
    } else {
        ""
    };
    Ok(format!(
        "set {} ({} cells){note}",
        range.a1(),
        range.cell_count()
    ))
}

/// Split `a, b, "c,d", e` on top-level commas.
fn split_list(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ',' => parts.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
    }
    parts.push(cur);
    parts.iter().map(|p| p.trim().to_string()).collect()
}

fn cmd_fill(session: &mut Session, rest: &str) -> Result<String> {
    let parts: Vec<&str> = rest.split("->").map(str::trim).collect();
    if parts.len() != 2 {
        return Err(Error::from("usage: fill <source> -> <target-range>"));
    }
    let src = session.resolve(parts[0])?;
    let dst = session.resolve(parts[1])?;
    if src.sheet_index != dst.sheet_index {
        return Err(Error::from(
            "fill source and target must be on the same sheet",
        ));
    }
    let sheet = src.sheet_index;
    let (s, d) = (src.range, dst.range);

    // Accept either a target containing the source (fill D2 -> D2:D100) or a
    // bare far end (fill D2 -> D100).
    let down = d.end.row > s.end.row && d.start.col >= s.start.col && d.end.col <= s.end.col;
    let right = d.end.col > s.end.col && d.start.row >= s.start.row && d.end.row <= s.end.row;
    match (down, right) {
        (true, false) => {
            session.book.auto_fill_rows(sheet, s, d.end.row)?;
            session.invalidate();
            Ok(format!(
                "filled {}:{} from {}",
                s.start.a1(),
                CellRef {
                    row: d.end.row,
                    col: s.end.col
                }
                .a1(),
                s.a1()
            ))
        }
        (false, true) => {
            session.book.auto_fill_columns(sheet, s, d.end.col)?;
            session.invalidate();
            Ok(format!(
                "filled {}:{} from {}",
                s.start.a1(),
                CellRef {
                    row: s.end.row,
                    col: d.end.col
                }
                .a1(),
                s.a1()
            ))
        }
        (false, false) => Err(Error::from(format!(
            "target {} does not extend source {} down or right",
            d.a1(),
            s.a1()
        ))),
        (true, true) => Err(Error::from(
            "target extends the source both down and right; fill one direction at a time",
        )),
    }
}

fn cmd_clear(session: &mut Session, rest: &str) -> Result<String> {
    let mut tokens = tokenize(rest);
    let what = match tokens.last().map(|s| s.to_ascii_lowercase()) {
        Some(w) if ["contents", "formats", "all"].contains(&w.as_str()) => {
            tokens.pop();
            w
        }
        _ => "contents".to_string(),
    };
    let target = tokens.join(" ");
    if target.is_empty() {
        return Err(Error::from("usage: clear <target> [contents|formats|all]"));
    }
    let r = session.resolve(&target)?;
    match what.as_str() {
        "contents" => session.book.clear_contents(r.sheet_index, r.range)?,
        "formats" => session.book.clear_formatting(r.sheet_index, r.range)?,
        "all" => session.book.clear_all(r.sheet_index, r.range)?,
        _ => unreachable!(),
    }
    session.invalidate();
    Ok(format!("cleared {} ({what})", r.qualified()))
}

fn cmd_structure(session: &mut Session, verb: &str, rest: &str) -> Result<String> {
    let mut tokens = tokenize(rest);
    if tokens.is_empty() {
        return Err(Error::from(format!(
            "usage: {verb} rows|cols <where> [count <n>] [in <sheet>]"
        )));
    }
    let axis = tokens.remove(0).to_ascii_lowercase();
    let sheet = parse_in_sheet(session, &mut tokens)?;
    let mut count: Option<i32> = None;
    if tokens.len() >= 2 && tokens[tokens.len() - 2].eq_ignore_ascii_case("count") {
        count = Some(
            tokens
                .pop()
                .unwrap()
                .parse()
                .map_err(|_| Error::from("count must be a number"))?,
        );
        tokens.pop();
    }
    let where_str = tokens.join("");
    if where_str.is_empty() {
        return Err(Error::from(format!("{verb} {axis}: missing position")));
    }

    match axis.as_str() {
        "rows" | "row" => {
            let (a, b) = parse_row_span(&where_str)?;
            let n = count.unwrap_or(b - a + 1);
            if verb == "insert" {
                session.book.insert_rows(sheet, a, n)?;
                session.invalidate();
                Ok(format!("inserted {n} row{} at {a}", plural(n)))
            } else {
                session.book.delete_rows(sheet, a, n)?;
                session.invalidate();
                Ok(format!("deleted {n} row{} from {a}", plural(n)))
            }
        }
        "cols" | "columns" | "col" => {
            let (a, b) = parse_col_span(&where_str)?;
            let n = count.unwrap_or(b - a + 1);
            let letter = number_to_column(a).unwrap_or_default();
            if verb == "insert" {
                session.book.insert_columns(sheet, a, n)?;
                session.invalidate();
                Ok(format!("inserted {n} column{} at {letter}", plural(n)))
            } else {
                session.book.delete_columns(sheet, a, n)?;
                session.invalidate();
                Ok(format!("deleted {n} column{} from {letter}", plural(n)))
            }
        }
        other => Err(Error::from(format!("expected rows|cols, got {other:?}"))),
    }
}

fn plural(n: i32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn parse_row_span(s: &str) -> Result<(i32, i32)> {
    let parse = |p: &str| -> Result<i32> {
        p.trim()
            .parse()
            .map_err(|_| Error::from(format!("bad row number {p:?}")))
    };
    match s.split_once(':') {
        None => {
            let a = parse(s)?;
            Ok((a, a))
        }
        Some((a, b)) => {
            let (a, b) = (parse(a)?, parse(b)?);
            Ok((a.min(b), a.max(b)))
        }
    }
}

fn parse_col_span(s: &str) -> Result<(i32, i32)> {
    let parse = |p: &str| -> Result<i32> {
        crate::addr::column_to_number(&p.trim().to_ascii_uppercase())
            .map_err(|_| Error::from(format!("bad column {p:?}")))
    };
    match s.split_once(':') {
        None => {
            let a = parse(s)?;
            Ok((a, a))
        }
        Some((a, b)) => {
            let (a, b) = (parse(a)?, parse(b)?);
            Ok((a.min(b), a.max(b)))
        }
    }
}

// ---- sort --------------------------------------------------------------------

fn cmd_sort(session: &mut Session, rest: &str, warnings: &mut Vec<String>) -> Result<String> {
    let lower = rest.to_lowercase();
    let by_pos = lower
        .find(" by ")
        .ok_or_else(|| Error::from("usage: sort <target> by <column> [asc|desc][, …]"))?;
    let target_str = rest[..by_pos].trim();
    let keys_str = rest[by_pos + 4..].trim();

    let resolved = session.resolve(target_str)?;
    let sheet = resolved.sheet_index;
    let range = resolved.range;

    // Find the matching region for header names and header-row exclusion.
    let (regions, _) = session_regions(session);
    let region = regions
        .iter()
        .find(|r| r.sheet == sheet && r.range == range)
        .or_else(|| {
            regions.iter().find(|r| {
                r.sheet == sheet
                    && r.range.start.row <= range.start.row
                    && r.range.end.row >= range.end.row
                    && r.range.start.col <= range.start.col
                    && r.range.end.col >= range.end.col
            })
        });

    let body_start = match region {
        Some(r) if r.header_row == Some(range.start.row) => range.start.row + 1,
        _ => range.start.row,
    };
    if body_start > range.end.row {
        return Err(Error::from("nothing to sort (only a header row)"));
    }

    // Parse sort keys.
    struct Key {
        col: i32,
        asc: bool,
    }
    let mut keys: Vec<Key> = Vec::new();
    for part in keys_str.split(',') {
        let tokens = tokenize(part.trim());
        if tokens.is_empty() {
            return Err(Error::from("empty sort key"));
        }
        let mut asc = true;
        let mut name_tokens: Vec<String> = Vec::new();
        for t in &tokens {
            match t.to_ascii_lowercase().as_str() {
                "asc" => asc = true,
                "desc" => asc = false,
                _ => name_tokens.push(unquote(t)),
            }
        }
        let key_name = name_tokens.join(" ");
        let col = match region.and_then(|r| r.column_by_key(&key_name)) {
            Some(c) => c,
            None => {
                crate::addr::column_to_number(&key_name.to_ascii_uppercase()).map_err(|_| {
                    Error::from(format!(
                        "sort key {key_name:?} is neither a header nor a column letter"
                    ))
                })?
            }
        };
        if col < range.start.col || col > range.end.col {
            return Err(Error::from(format!(
                "sort key column {} is outside {}",
                number_to_column(col).unwrap_or_default(),
                range.a1()
            )));
        }
        keys.push(Key { col, asc });
    }
    if keys.is_empty() {
        return Err(Error::from("no sort keys given"));
    }

    // Read the body: contents (for rewriting) and values (for comparing).
    let rows: Vec<i32> = (body_start..=range.end.row).collect();
    let mut contents: HashMap<(i32, i32), String> = HashMap::new();
    let mut values: HashMap<(i32, i32), Value> = HashMap::new();
    for &row in &rows {
        for col in range.start.col..=range.end.col {
            let content = session.book.content(sheet, row, col)?;
            if !content.is_empty() {
                contents.insert((row, col), content);
            }
            values.insert((row, col), session.book.value(sheet, row, col));
        }
    }

    // Order rows.
    let mut order: Vec<i32> = rows.clone();
    order.sort_by(|&ra, &rb| {
        for k in &keys {
            let va = values.get(&(ra, k.col)).unwrap_or(&Value::Empty);
            let vb = values.get(&(rb, k.col)).unwrap_or(&Value::Empty);
            // Empties sort last regardless of direction.
            let ord = match (va.is_empty(), vb.is_empty()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (true, true) => std::cmp::Ordering::Equal,
                (false, false) => {
                    let ord = compare_values(va, vb);
                    if k.asc {
                        ord
                    } else {
                        ord.reverse()
                    }
                }
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        ra.cmp(&rb) // stable fallback
    });

    if order == rows {
        return Ok(format!("{} already sorted — no change", range.a1()));
    }

    // Rewrite rows into their new positions, re-anchoring formulas.
    let mut moved_formulas = 0usize;
    session.book.batch(|b| {
        for (i, &src_row) in order.iter().enumerate() {
            let dst_row = body_start + i as i32;
            for col in range.start.col..=range.end.col {
                match contents.get(&(src_row, col)) {
                    None => {
                        b.clear_contents(sheet, Range::cell(CellRef { row: dst_row, col }))?;
                    }
                    Some(c) => {
                        let v = if c.starts_with('=') && dst_row != src_row {
                            moved_formulas += 1;
                            displace_rows(c, dst_row - src_row)
                        } else {
                            c.clone()
                        };
                        b.set_input(sheet, dst_row, col, &v)?;
                    }
                }
            }
        }
        Ok(())
    })?;
    session.invalidate();
    if moved_formulas > 0 {
        warnings.push(format!(
            "{moved_formulas} formula cell{} moved and re-anchored to their new rows (relative references follow the row; absolute $ references stay)",
            if moved_formulas == 1 { "" } else { "s" }
        ));
    }
    let key_desc: Vec<String> = keys
        .iter()
        .map(|k| {
            format!(
                "{}{}",
                number_to_column(k.col).unwrap_or_default(),
                if k.asc { "" } else { " desc" }
            )
        })
        .collect();
    Ok(format!(
        "sorted {} rows {}..{} by {}",
        range.a1(),
        body_start,
        range.end.row,
        key_desc.join(", ")
    ))
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            Value::Error(_) => 3,
            Value::Empty => 4,
        }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Error(x), Value::Error(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

// ---- find ---------------------------------------------------------------------

fn cmd_find(session: &mut Session, rest: &str) -> Result<String> {
    let mut tokens = tokenize(rest);
    if tokens.is_empty() {
        return Err(Error::from(
            "usage: find \"<text>\" [in <sheet or range>] [values|formulas]",
        ));
    }
    let needle = unquote(&tokens.remove(0)).to_lowercase();
    let mut mode_formulas = false;
    let mut scope: Option<String> = None;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].to_ascii_lowercase().as_str() {
            "values" => {
                tokens.remove(i);
            }
            "formulas" => {
                mode_formulas = true;
                tokens.remove(i);
            }
            "in" => {
                tokens.remove(i);
                if i < tokens.len() {
                    scope = Some(unquote(&tokens.remove(i)));
                }
            }
            _ => i += 1,
        }
    }

    let scope_resolved = match &scope {
        Some(s) => Some(session.resolve(s)?),
        None => None,
    };

    let mut hits: Vec<String> = Vec::new();
    let mut total = 0usize;
    const CAP: usize = 50;
    let sheets: Vec<u32> = match &scope_resolved {
        Some(r) => vec![r.sheet_index],
        None => (0..session.book.sheet_count()).collect(),
    };
    let multi_sheet = session.book.sheet_count() > 1;
    for sheet in sheets {
        let sheet_name = session.book.sheet_names()[sheet as usize].clone();
        let mut cells: Vec<(i32, i32)> = Vec::new();
        session.book.for_each_cell(sheet, |row, col, _| {
            if let Some(r) = &scope_resolved {
                if !r.range.contains(row, col) {
                    return;
                }
            }
            cells.push((row, col));
        });
        cells.sort_unstable();
        for (row, col) in cells {
            let matched = if mode_formulas {
                session
                    .book
                    .formula(sheet, row, col)
                    .ok()
                    .flatten()
                    .is_some_and(|f| f.to_lowercase().contains(&needle))
            } else {
                let v = session.book.value(sheet, row, col);
                v.display().to_lowercase().contains(&needle)
                    || session
                        .book
                        .formatted_value(sheet, row, col)
                        .to_lowercase()
                        .contains(&needle)
            };
            if matched {
                total += 1;
                if hits.len() < CAP {
                    let addr = if multi_sheet {
                        format!(
                            "{}!{}",
                            display_sheet(&sheet_name),
                            CellRef { row, col }.a1()
                        )
                    } else {
                        CellRef { row, col }.a1()
                    };
                    let formula = session.book.formula(sheet, row, col).ok().flatten();
                    let value = session.book.value(sheet, row, col);
                    let text = match formula {
                        Some(f) => format!("{f} ⇒ {}", value.display()),
                        None => value.display(),
                    };
                    hits.push(format!("{addr}: {text}"));
                }
            }
        }
    }
    if total == 0 {
        return Ok(format!(
            "no {} match {needle:?}",
            if mode_formulas { "formulas" } else { "values" }
        ));
    }
    let mut out = format!("{total} match{}:\n", if total == 1 { "" } else { "es" });
    out.push_str(&hits.join("\n"));
    if total > CAP {
        out.push_str(&format!(
            "\n… {} more (narrow with `in <range>`)",
            total - CAP
        ));
    }
    Ok(out)
}

// ---- names, sheets ---------------------------------------------------------------

fn cmd_name(session: &mut Session, rest: &str) -> Result<String> {
    let parts: Vec<&str> = rest.splitn(2, '=').map(str::trim).collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(Error::from("usage: name <ident> = <range>"));
    }
    let ident = parts[0].to_string();
    let resolved = session.resolve(parts[1])?;
    // Defined-name formulas use absolute references.
    let formula = format!(
        "{}!${}${}:${}${}",
        display_sheet(&resolved.sheet_name),
        number_to_column(resolved.range.start.col).unwrap_or_default(),
        resolved.range.start.row,
        number_to_column(resolved.range.end.col).unwrap_or_default(),
        resolved.range.end.row,
    );
    session.book.define_name(&ident, &formula)?;
    session.invalidate();
    Ok(format!("named {ident} = {}", resolved.qualified()))
}

fn cmd_unname(session: &mut Session, rest: &str) -> Result<String> {
    let ident = rest.trim();
    if ident.is_empty() {
        return Err(Error::from("usage: unname <ident>"));
    }
    session.book.delete_defined_name(ident)?;
    session.invalidate();
    Ok(format!("removed defined name {ident}"))
}

fn cmd_names(session: &mut Session) -> Result<String> {
    let names = session.book.defined_names();
    if names.is_empty() {
        return Ok("no defined names".to_string());
    }
    Ok(names
        .iter()
        .map(|(n, _s, f)| format!("{n} = {f}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn cmd_sheet(session: &mut Session, rest: &str) -> Result<String> {
    let (sub, rest) = split_word(rest);
    match sub.to_ascii_lowercase().as_str() {
        "new" => {
            let name = unquote(rest);
            if name.is_empty() {
                return Err(Error::from("usage: sheet new \"<name>\""));
            }
            session.book.add_sheet(&name)?;
            session.current_sheet = session.book.sheet_count() - 1;
            session.invalidate();
            Ok(format!("created sheet {name:?} (now current)"))
        }
        "select" => {
            let name = unquote(rest);
            let idx = session
                .book
                .sheet_index(&name)
                .ok_or_else(|| Error::from(format!("no sheet named {name:?}")))?;
            session.current_sheet = idx;
            Ok(format!("current sheet: {name}"))
        }
        "rename" => {
            let parts: Vec<&str> = rest.split("->").map(str::trim).collect();
            if parts.len() != 2 {
                return Err(Error::from("usage: sheet rename \"<old>\" -> \"<new>\""));
            }
            let (old, new) = (unquote(parts[0]), unquote(parts[1]));
            let idx = session
                .book
                .sheet_index(&old)
                .ok_or_else(|| Error::from(format!("no sheet named {old:?}")))?;
            session.book.rename_sheet(idx, &new)?;
            session.invalidate();
            Ok(format!("renamed sheet {old:?} -> {new:?}"))
        }
        "delete" => {
            let name = unquote(rest);
            let idx = session
                .book
                .sheet_index(&name)
                .ok_or_else(|| Error::from(format!("no sheet named {name:?}")))?;
            session.book.delete_sheet(idx)?;
            if session.current_sheet >= session.book.sheet_count() {
                session.current_sheet = 0;
            }
            session.invalidate();
            Ok(format!("deleted sheet {name:?}"))
        }
        other => Err(Error::from(format!(
            "unknown subcommand `sheet {other}` (new, select, rename, delete)"
        ))),
    }
}

// ---- history ------------------------------------------------------------------

fn cmd_checkpoint(session: &mut Session, rest: &str) -> Result<String> {
    let name = rest.trim();
    if name.is_empty() {
        return Err(Error::from("usage: checkpoint <name>"));
    }
    session.checkpoint(name);
    Ok(format!("checkpoint {name:?} saved"))
}

fn cmd_restore(session: &mut Session, rest: &str) -> Result<String> {
    let name = rest.trim();
    if name.is_empty() {
        return Err(Error::from("usage: restore <name>"));
    }
    session.restore(name)?;
    Ok(format!("restored checkpoint {name:?} (undo history reset)"))
}

fn cmd_undo(session: &mut Session) -> Result<String> {
    if session.book.can_undo() {
        session.book.undo()?;
        session.invalidate();
        return Ok("undone".to_string());
    }
    // Engine history is empty (fresh or rehydrated session). Fall back to the
    // journal when the server layer installed one.
    let Some(history) = &session.history else {
        return Err(Error::from("nothing to undo"));
    };
    if session.needs_resync {
        // The journal's notion of "previous state" only advances when an exec
        // completes, so a second journal undo inside the same script would
        // rebuild the same state.
        return Err(Error::from(
            "one journal undo per exec — run further undos in a separate call",
        ));
    }
    match history.state_back(1)? {
        Some(book) => {
            session.book = book;
            session.needs_resync = true;
            session.invalidate();
            Ok("undone (rebuilt from the exec journal)".to_string())
        }
        None => Err(Error::from(
            "undo history does not reach back that far (journal compacted); use `restore <checkpoint>` instead",
        )),
    }
}

fn cmd_redo(session: &mut Session) -> Result<String> {
    if !session.book.can_redo() {
        return Err(Error::from("nothing to redo"));
    }
    session.book.redo()?;
    session.invalidate();
    Ok("redone".to_string())
}

// ---- expect ---------------------------------------------------------------------

fn cmd_expect(session: &mut Session, rest: &str) -> Result<String> {
    let ops = ["==", "!=", ">=", "<=", ">", "<"];
    let (op, pos) = ops
        .iter()
        .filter_map(|op| rest.find(op).map(|p| (*op, p)))
        .min_by_key(|(op, p)| (*p, std::cmp::Reverse(op.len())))
        .ok_or_else(|| Error::from("usage: expect <cell> <==|!=|>|>=|<|<=> <value>"))?;
    let target = rest[..pos].trim();
    let expected_str = rest[pos + op.len()..].trim();
    let (resolved, cell) = resolve_cell(session, target)?;
    let actual = session.book.value(resolved.sheet_index, cell.row, cell.col);

    let pass = match (op, &actual) {
        ("==", _) | ("!=", _) => {
            let eq = value_equals(&actual, expected_str);
            if op == "==" {
                eq
            } else {
                !eq
            }
        }
        (_, Value::Number(n)) => {
            let e: f64 = expected_str
                .parse()
                .map_err(|_| Error::from(format!("{expected_str:?} is not a number")))?;
            match op {
                ">" => *n > e,
                ">=" => *n >= e,
                "<" => *n < e,
                "<=" => *n <= e,
                _ => unreachable!(),
            }
        }
        _ => {
            return Err(Error::from(format!(
                "cannot compare {} value with {op}",
                type_name(&actual)
            )))
        }
    };
    if pass {
        Ok(format!(
            "expect {target} {op} {expected_str}: OK (actual {})",
            actual.display()
        ))
    } else {
        Err(Error::from(format!(
            "expectation failed: {target} {op} {expected_str}, actual {}",
            actual.display()
        )))
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Empty => "empty",
        Value::Number(_) => "number",
        Value::Text(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Error(_) => "error",
    }
}

fn value_equals(actual: &Value, expected: &str) -> bool {
    let expected = expected.trim();
    match actual {
        Value::Number(n) => expected
            .parse::<f64>()
            .map(|e| (n - e).abs() <= 1e-9 * n.abs().max(e.abs()).max(1.0))
            .unwrap_or(false),
        Value::Text(s) => unquote(expected) == *s,
        Value::Bool(b) => expected.eq_ignore_ascii_case(if *b { "true" } else { "false" }),
        Value::Error(e) => expected == e,
        Value::Empty => {
            expected.is_empty() || expected == "\"\"" || expected.eq_ignore_ascii_case("empty")
        }
    }
}

// ---- highlights --------------------------------------------------------------------

fn cmd_highlight(session: &mut Session, rest: &str, author: &str) -> Result<String> {
    let mut tokens = tokenize(rest);
    if tokens.is_empty() {
        return Err(Error::from(
            "usage: highlight <range> [color=<c>] [note=\"…\"]",
        ));
    }
    let mut color = "amber".to_string();
    let mut note: Option<String> = None;
    let mut target_parts = Vec::new();
    for t in tokens.drain(..) {
        if let Some(v) = t.strip_prefix("color=") {
            color = unquote(v);
        } else if let Some(v) = t.strip_prefix("note=") {
            note = Some(unquote(v));
        } else {
            target_parts.push(t);
        }
    }
    let target = target_parts.join(" ");
    let r = session.resolve(&target)?;
    let id = session.add_highlight(r.sheet_index, &r.sheet_name, r.range, &color, note, author);
    Ok(format!("highlight #{id} on {} ({color})", r.qualified()))
}

fn cmd_unhighlight(session: &mut Session, rest: &str) -> Result<String> {
    let id: u32 = rest
        .trim()
        .trim_start_matches('#')
        .parse()
        .map_err(|_| Error::from("usage: unhighlight <id>"))?;
    if session.remove_highlight(id) {
        Ok(format!("removed highlight #{id}"))
    } else {
        Err(Error::from(format!("no highlight #{id}")))
    }
}

fn cmd_highlights(session: &mut Session) -> Result<String> {
    if session.highlights.is_empty() {
        return Ok("no highlights".to_string());
    }
    let lines: Vec<String> = session
        .highlights
        .iter()
        .map(|h| {
            format!(
                "#{} {}!{} {} by {}{}",
                h.id,
                display_sheet(&h.sheet_name),
                h.range.a1(),
                h.color,
                h.author,
                h.note
                    .as_ref()
                    .map(|n| format!(" — {n:?}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Book;

    fn orders_session() -> Session {
        let mut book = Book::new_empty("demo").unwrap();
        book.batch(|b| {
            b.set_input(0, 1, 1, "Item")?;
            b.set_input(0, 1, 2, "Qty")?;
            b.set_input(0, 1, 3, "Price")?;
            let rows = [("Bee", 10, 0.4), ("Ape", 2, 3.5), ("Cat", 1, 12.0)];
            for (i, (item, qty, price)) in rows.iter().enumerate() {
                let row = i as i32 + 2;
                b.set_input(0, row, 1, item)?;
                b.set_input(0, row, 2, &qty.to_string())?;
                b.set_input(0, row, 3, &price.to_string())?;
            }
            Ok(())
        })
        .unwrap();
        Session::new(book, None)
    }

    fn run(s: &mut Session, script: &str) -> ExecOutput {
        exec(s, script, "agent")
    }

    #[test]
    fn set_fill_expect_pipeline() {
        let mut s = orders_session();
        let out = run(
            &mut s,
            "set D1 Total\nset D2 =B2*C2\nfill D2 -> D2:D4\nexpect D4 == 12",
        );
        assert!(out.ok(), "{:?}", out.failed);
        assert_eq!(s.book.value(0, 3, 4), Value::Number(7.0)); // D3 = 2*3.5
        assert!(!out.delta.is_empty());
    }

    #[test]
    fn set_list_batch() {
        let mut s = orders_session();
        let out = run(&mut s, "set B2:B4 = [5, 5, 5]");
        assert!(out.ok(), "{:?}", out.failed);
        assert_eq!(s.book.value(0, 4, 2), Value::Number(5.0));
        // wrong arity fails
        let out = run(&mut s, "set B2:B4 = [1, 2]");
        assert!(!out.ok());
    }

    #[test]
    fn sort_by_header_reanchors_formulas() {
        let mut s = orders_session();
        let out = run(
            &mut s,
            "set D1 Total\nset D2:D4 =B2*C2\nsort table1 by Price desc",
        );
        assert!(out.ok(), "{:?}", out.failed);
        // Price desc: Cat(12), Ape(3.5), Bee(0.4)
        assert_eq!(s.book.value(0, 2, 1), Value::Text("Cat".into()));
        assert_eq!(s.book.value(0, 4, 1), Value::Text("Bee".into()));
        // Formula on row 2 must now compute Cat's total: 1*12
        assert_eq!(s.book.value(0, 2, 4), Value::Number(12.0));
        assert_eq!(s.book.formula(0, 2, 4).unwrap().as_deref(), Some("=B2*C2"));
        assert!(out.warnings.iter().any(|w| w.contains("re-anchored")));
    }

    #[test]
    fn failing_line_stops_script() {
        let mut s = orders_session();
        let out = run(&mut s, "set B2 99\nexpect B2 == 1\nset B3 77");
        assert!(!out.ok());
        let (line, _, _) = out.failed.as_ref().unwrap();
        assert_eq!(*line, 2);
        // line 1 applied, line 3 did not run
        assert_eq!(s.book.value(0, 2, 2), Value::Number(99.0));
        assert_eq!(s.book.value(0, 3, 2), Value::Number(2.0));
    }

    #[test]
    fn find_values_and_formulas() {
        let mut s = orders_session();
        run(&mut s, "set D2 =B2*C2");
        let out = run(&mut s, "find \"ape\"");
        assert!(out.ok());
        assert!(out.results[0].contains("A3"), "{}", out.results[0]);
        let out = run(&mut s, "find \"B2\" formulas");
        assert!(out.results[0].contains("D2"), "{}", out.results[0]);
    }

    #[test]
    fn checkpoint_restore_undo() {
        let mut s = orders_session();
        let out = run(
            &mut s,
            "checkpoint start\nset B2 1000\nrestore start\nexpect B2 == 10",
        );
        assert!(out.ok(), "{:?}", out.failed);
        // The delta nets out to nothing: set then restore.
        assert!(out.delta.is_empty(), "{:?}", out.delta);
        let out = run(&mut s, "set B2 7\nundo\nexpect B2 == 10");
        assert!(out.ok(), "{:?}", out.failed);
    }

    #[test]
    fn structure_ops() {
        let mut s = orders_session();
        let out = run(&mut s, "insert rows 2 count 2\nexpect A4 == \"Bee\"");
        assert!(out.ok(), "{:?}", out.failed);
        let out = run(&mut s, "delete rows 2:3\nexpect A2 == \"Bee\"");
        assert!(out.ok(), "{:?}", out.failed);
        let out = run(&mut s, "insert cols B count 1\nexpect C2 == 10");
        assert!(out.ok(), "{:?}", out.failed);
        let out = run(&mut s, "delete cols B");
        assert!(out.ok(), "{:?}", out.failed);
        assert_eq!(s.book.value(0, 2, 2), Value::Number(10.0));
    }

    #[test]
    fn names_and_sheets() {
        let mut s = orders_session();
        let out = run(
            &mut s,
            "name orders = A1:C4\nsheet new \"Summary\"\nset A1 =SUM(Sheet1!B2:B4)\nexpect A1 == 13",
        );
        assert!(out.ok(), "{:?}", out.failed);
        let out = run(&mut s, "view orders");
        assert!(out.ok(), "{:?}", out.failed);
        assert!(
            out.results[0].contains("Sheet1!A1:C4"),
            "{}",
            out.results[0]
        );
    }

    #[test]
    fn highlights_roundtrip() {
        let mut s = orders_session();
        let out = run(&mut s, "highlight B2 color=red note=\"check\"\nhighlights");
        assert!(out.ok(), "{:?}", out.failed);
        assert!(out.results[1].contains("#1"), "{}", out.results[1]);
        assert!(out.results[1].contains("check"));
        let out = run(&mut s, "unhighlight 1\nhighlights");
        assert!(out.results[1].contains("no highlights"));
    }

    #[test]
    fn view_switches_current_sheet() {
        let mut s = orders_session();
        run(&mut s, "sheet new \"Other\"\nset A1 42");
        let out = run(&mut s, "view Sheet1!A1:C2\nset A1 replaced");
        assert!(out.ok(), "{:?}", out.failed);
        assert_eq!(s.book.value(0, 1, 1), Value::Text("replaced".into()));
        assert_eq!(s.book.value(1, 1, 1), Value::Number(42.0));
    }

    #[test]
    fn undo_falls_back_to_history_provider() {
        use crate::session::HistoryProvider;
        struct Fixed(Vec<u8>);
        impl HistoryProvider for Fixed {
            fn state_back(&self, _steps: u32) -> crate::Result<Option<Book>> {
                Ok(Some(Book::from_bytes(&self.0)?))
            }
        }
        // Previous state: A1 = 5.
        let mut prev = Book::new_empty("t").unwrap();
        prev.set_input(0, 1, 1, "5").unwrap();
        let prev_bytes = prev.to_bytes();
        // Current state, rehydrated (no engine history): A1 = 9.
        let mut cur = Book::new_empty("t").unwrap();
        cur.set_input(0, 1, 1, "9").unwrap();
        let mut s = Session::new(Book::from_bytes(&cur.to_bytes()).unwrap(), None);
        assert!(!s.book.can_undo(), "rehydrated book has no engine history");

        // Without a provider: plain error.
        let out = run(&mut s, "undo");
        assert!(!out.ok());

        s.history = Some(Box::new(Fixed(prev_bytes)));
        let out = run(&mut s, "undo\nexpect A1 == 5");
        assert!(out.ok(), "{:?}", out.failed);
        assert!(out.needs_resync, "book swap flagged for replicas");
        assert!(out.results[0].contains("journal"), "{}", out.results[0]);

        // A second journal undo inside the same exec is refused; a fresh
        // exec may undo again.
        let out = run(&mut s, "undo\nundo");
        assert!(!out.ok());
        assert!(out.failed.as_ref().unwrap().2.contains("separate call"));
    }

    #[test]
    fn restore_flags_resync() {
        let mut s = orders_session();
        let out = run(
            &mut s,
            "checkpoint a\nset B2 77\nrestore a\nexpect B2 == 10",
        );
        assert!(out.ok(), "{:?}", out.failed);
        assert!(out.needs_resync);
    }

    #[test]
    fn delta_reports_ripple() {
        let mut s = orders_session();
        run(&mut s, "set D2 =B2*C2");
        let out = run(&mut s, "set B2 100");
        let rendered = out.render(false);
        assert!(rendered.contains("B2 10 ⇒ 100"), "{rendered}");
        assert!(rendered.contains("D2 4 ⇒ 40"), "{rendered}");
    }
}
