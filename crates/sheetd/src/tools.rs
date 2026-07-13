//! The five workbook tools, shared by every transport. Thin orchestration
//! over `sheetkit`: sessions in, rendered text out.

use serde_json::{json, Value as Json};
use sheetkit::book::Book;
use sheetkit::cmd;
use sheetkit::session::{Manager, Session};
use sheetkit::view::{self, Mode, ViewOptions};
use sheetkit::{Error, Result};

pub struct Tools {
    pub manager: Manager,
}

impl Tools {
    pub fn new() -> Tools {
        Tools { manager: Manager::new() }
    }

    /// `sheet_open {path | new}` → workbook id + structure sketch.
    pub fn open(&mut self, path: Option<&str>, new: Option<&str>) -> Result<String> {
        let (book, origin) = match (path, new) {
            (Some(p), None) => (Book::open(p)?, Some(p.to_string())),
            (None, name) => (Book::new_empty(name.unwrap_or("workbook"))?, None),
            (Some(_), Some(_)) => {
                return Err(Error::from("pass either `path` or `new`, not both"))
            }
        };
        let mut session = Session::new(book, origin.clone());
        let (regions, _) = session.regions().clone();
        let sketch = view::sketch(&session.book, &regions);
        let id = self.manager.insert(session);
        let origin_note = origin.map(|p| format!(" from {p}")).unwrap_or_default();
        Ok(format!("opened {id}{origin_note}\n\n{sketch}"))
    }

    /// `sheet_exec {workbook_id, script}` → results + recalc echo.
    pub fn exec(&mut self, workbook_id: &str, script: &str, author: &str) -> Result<String> {
        let session = self.manager.get_mut(workbook_id)?;
        let out = cmd::exec(session, script, author);
        let multi_sheet = session.book.sheet_count() > 1;
        Ok(out.render(multi_sheet))
    }

    /// `sheet_view {workbook_id, target, mode?, budget_tokens?}`.
    pub fn view(
        &mut self,
        workbook_id: &str,
        target: &str,
        mode: Option<&str>,
        budget_tokens: Option<usize>,
    ) -> Result<String> {
        let session = self.manager.get_mut(workbook_id)?;
        let mut opts = ViewOptions::default();
        if let Some(m) = mode {
            opts.mode = Some(match m {
                "dense" => Mode::Dense,
                "agg" | "aggregated" => Mode::Aggregated,
                "sparse" => Mode::Sparse,
                other => return Err(Error::from(format!("unknown mode {other:?}"))),
            });
        }
        if let Some(b) = budget_tokens {
            opts.budget_tokens = b;
        }
        let resolved = session.resolve(target)?;
        session.current_sheet = resolved.sheet_index;
        let (regions, _) = session.regions().clone();
        Ok(view::render_view(&session.book, &resolved, &regions, opts))
    }

    /// `sheet_save {workbook_id, path?, overwrite?}`. With no path, saves back
    /// to where the workbook was opened from.
    pub fn save(&mut self, workbook_id: &str, path: Option<&str>, overwrite: bool) -> Result<String> {
        let session = self.manager.get_mut(workbook_id)?;
        let (target, implicit) = match (path, &session.origin) {
            (Some(p), _) => (p.to_string(), false),
            (None, Some(o)) => (o.clone(), true),
            (None, None) => {
                return Err(Error::from(
                    "this workbook was created fresh; pass `path` to say where to save it",
                ))
            }
        };
        // Saving back to the origin is a plain "save" — overwrite implied.
        session.book.save(&target, overwrite || implicit)?;
        Ok(format!("saved {workbook_id} to {target}"))
    }

    /// `sheet_close {workbook_id}`.
    pub fn close(&mut self, workbook_id: &str) -> Result<String> {
        self.manager.close(workbook_id)?;
        Ok(format!("closed {workbook_id}"))
    }

    /// Dispatch an MCP tool call by name with JSON arguments.
    pub fn call(&mut self, name: &str, args: &Json) -> Result<String> {
        let s = |key: &str| args.get(key).and_then(Json::as_str);
        match name {
            "sheet_open" => self.open(s("path"), s("new")),
            "sheet_exec" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                let script = s("script").ok_or(Error::from("script is required"))?;
                self.exec(id, script, "agent")
            }
            "sheet_view" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                let target = s("target").ok_or(Error::from("target is required"))?;
                let budget = args.get("budget_tokens").and_then(Json::as_u64).map(|b| b as usize);
                self.view(id, target, s("mode"), budget)
            }
            "sheet_save" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                let overwrite = args.get("overwrite").and_then(Json::as_bool).unwrap_or(false);
                self.save(id, s("path"), overwrite)
            }
            "sheet_close" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                self.close(id)
            }
            other => Err(Error::from(format!("unknown tool {other:?}"))),
        }
    }
}

/// MCP tool definitions: few tools, wide verbs. The DSL carries the surface
/// area; the schema stays small so models keep the whole contract in view.
pub fn tool_definitions() -> Json {
    json!([
        {
            "name": "sheet_open",
            "description": "Open a spreadsheet (xlsx, csv, or ic) or create a new one. Returns a workbook_id plus a structure sketch: every sheet, detected table regions with per-column types/ranges/fill formulas, and defined names. Read the sketch instead of dumping cells — it is usually all you need to start working.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to open (.xlsx, .csv, .ic)" },
                    "new": { "type": "string", "description": "Create a fresh workbook with this name instead of opening a file" }
                }
            }
        },
        {
            "name": "sheet_exec",
            "description": "Run a script of spreadsheet commands, one per line, against an open workbook. Commands: view/set/fill/clear, insert|delete rows|cols, sort … by …, find, name, sheet new|rename|delete, checkpoint/restore/undo/redo, expect (assert a cell value — use after non-trivial edits), highlight. Targets are A1 ranges, region names from the sketch, or defined names. Every run returns a recalc delta: exactly which cells changed, old ⇒ new — trust it instead of re-reading. A failing line stops the script; earlier lines stay applied. Run `help` for full syntax.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workbook_id": { "type": "string" },
                    "script": { "type": "string", "description": "One command per line. Example:\nset D1 Total\nset D2 =B2*C2\nfill D2 -> D2:D100\nexpect D100 == 42" }
                },
                "required": ["workbook_id", "script"]
            }
        },
        {
            "name": "sheet_view",
            "description": "Read a range, region, or sheet as text. Small ranges render dense (grid with formulas AND computed values); large regions render aggregated (per-column type, min..max, distinct count, fill formulas, deviations). Any truncation is announced explicitly. Prefer region names over guessing A1 bounds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workbook_id": { "type": "string" },
                    "target": { "type": "string", "description": "A1 range (Sheet2!A1:C10), region name, defined name, or sheet name" },
                    "mode": { "type": "string", "enum": ["dense", "aggregated", "sparse"], "description": "Force an encoding (default: auto by size)" },
                    "budget_tokens": { "type": "integer", "description": "Approximate output budget (default 2000)" }
                },
                "required": ["workbook_id", "target"]
            }
        },
        {
            "name": "sheet_save",
            "description": "Save the workbook. Without a path, saves back to the file it was opened from. Format follows the extension: .xlsx, .csv, .ic. Saving to a NEW existing path requires overwrite=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workbook_id": { "type": "string" },
                    "path": { "type": "string" },
                    "overwrite": { "type": "boolean" }
                },
                "required": ["workbook_id"]
            }
        },
        {
            "name": "sheet_close",
            "description": "Close an open workbook and drop its session (unsaved changes are lost).",
            "inputSchema": {
                "type": "object",
                "properties": { "workbook_id": { "type": "string" } },
                "required": ["workbook_id"]
            }
        }
    ])
}
