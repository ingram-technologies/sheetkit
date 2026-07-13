//! The workbook tools, shared by every transport (stdio MCP, HTTP MCP, REST,
//! WebSocket). Thin orchestration over `sheetkit`: sessions in, rendered text
//! out, with optional blob persistence and an observer for realtime fan-out.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use serde_json::{json, Value as Json};
use sheetkit::book::Book;
use sheetkit::cmd;
use sheetkit::session::{Manager, Session};
use sheetkit::view::{self, Mode, ViewOptions};
use sheetkit::{Error, Result};

/// What one script run did — everything a realtime channel needs to fan out.
pub struct ExecEvent {
    pub workbook_id: String,
    pub principal: String,
    pub cmd_id: Option<String>,
    pub ok: bool,
    /// Per-command result lines (already human/model readable).
    pub summary: String,
    /// Changed cells, capped at [`EVENT_DELTA_CAP`]: `(addr, old, new)`.
    pub delta: Vec<(String, String, String)>,
    pub delta_total: usize,
    /// The engine's opaque diff blob for same-version replicas (may be empty).
    pub diffs: Vec<u8>,
    /// `(line number, message)` when the script stopped early.
    pub error: Option<(usize, String)>,
}

pub const EVENT_DELTA_CAP: usize = 200;

/// Observer for realtime transports; no-op default methods keep stdio simple.
pub trait ExecObserver: Send {
    fn exec_started(&self, _workbook_id: &str, _principal: &str, _script: &str) {}
    fn exec_finished(&self, _event: &ExecEvent) {}
}

pub struct Tools {
    pub manager: Manager,
    /// Blob store directory: every mutation persists `{id}.ic`, and unknown
    /// ids rehydrate from it (server mode).
    pub data_dir: Option<PathBuf>,
    /// Server mode uses stable random ids instead of the wb1/wb2 counter.
    pub random_ids: bool,
    pub observer: Option<Box<dyn ExecObserver>>,
    id_salt: u64,
}

impl Tools {
    pub fn new() -> Tools {
        Tools {
            manager: Manager::new(),
            data_dir: None,
            random_ids: false,
            observer: None,
            id_salt: 0,
        }
    }

    fn new_id(&mut self) -> String {
        self.id_salt += 1;
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .hash(&mut h);
        self.id_salt.hash(&mut h);
        std::process::id().hash(&mut h);
        format!("wb-{:08x}", (h.finish() & 0xffff_ffff) as u32)
    }

    fn insert(&mut self, session: Session) -> String {
        let id = if self.random_ids {
            let id = self.new_id();
            self.manager.insert_with_id(&id, session);
            id
        } else {
            self.manager.insert(session)
        };
        self.persist(&id);
        id
    }

    /// Get a session, rehydrating from the blob store when needed.
    pub fn session(&mut self, id: &str) -> Result<&mut Session> {
        if !self.manager.contains(id) {
            if let Some(dir) = &self.data_dir {
                let path = dir.join(format!("{id}.ic"));
                if let Ok(bytes) = std::fs::read(&path) {
                    let book = Book::from_bytes(&bytes)?;
                    self.manager
                        .insert_with_id(id, Session::new(book, Some(path.display().to_string())));
                }
            }
        }
        self.manager.get_mut(id)
    }

    /// Best-effort blob persistence; failures are logged, not fatal.
    fn persist(&mut self, id: &str) {
        let Some(dir) = self.data_dir.clone() else {
            return;
        };
        let Ok(session) = self.manager.get_mut(id) else {
            return;
        };
        let bytes = session.book.to_bytes();
        let path = dir.join(format!("{id}.ic"));
        if let Err(e) = std::fs::write(&path, bytes) {
            eprintln!("sheetd: failed to persist {id} to {}: {e}", path.display());
        }
    }

    fn sketch_text(session: &mut Session) -> String {
        let (regions, _) = session.regions().clone();
        view::sketch(&session.book, &regions)
    }

    // ---- the five tools -----------------------------------------------------

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
        let sketch = Self::sketch_text(&mut session);
        let id = self.insert(session);
        let origin_note = origin.map(|p| format!(" from {p}")).unwrap_or_default();
        Ok(format!("opened {id}{origin_note}\n\n{sketch}"))
    }

    /// Open from raw bytes (REST door). Format: `xlsx`, `csv`, or `ic`;
    /// empty bytes create a fresh workbook.
    pub fn open_bytes(&mut self, name: &str, format: &str, bytes: &[u8]) -> Result<String> {
        let book = if bytes.is_empty() {
            Book::new_empty(name)?
        } else {
            match format {
                "xlsx" => Book::from_xlsx_bytes(bytes, name)?,
                "csv" => Book::from_csv_str(
                    std::str::from_utf8(bytes)
                        .map_err(|_| Error::from("csv body is not valid UTF-8"))?,
                    name,
                )?,
                "ic" => Book::from_bytes(bytes)?,
                other => {
                    return Err(Error::from(format!(
                        "unknown format {other:?} (xlsx, csv or ic)"
                    )))
                }
            }
        };
        Ok(self.insert(Session::new(book, None)))
    }

    /// Replace an open workbook's content in place, keeping its id.
    pub fn replace_bytes(&mut self, id: &str, format: &str, bytes: &[u8]) -> Result<()> {
        let book = match format {
            "xlsx" => Book::from_xlsx_bytes(bytes, id)?,
            "csv" => Book::from_csv_str(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::from("csv body is not valid UTF-8"))?,
                id,
            )?,
            "ic" => Book::from_bytes(bytes)?,
            other => {
                return Err(Error::from(format!(
                    "unknown format {other:?} (xlsx, csv or ic)"
                )))
            }
        };
        let session = self.session(id)?;
        session.book = book;
        session.invalidate();
        self.persist(id);
        Ok(())
    }

    /// Export an open workbook as bytes.
    pub fn export_bytes(&mut self, id: &str, format: &str) -> Result<Vec<u8>> {
        let session = self.session(id)?;
        match format {
            "xlsx" => session.book.to_xlsx_bytes(),
            "csv" => Ok(session.book.to_csv(session.current_sheet)?.into_bytes()),
            "ic" => Ok(session.book.to_bytes()),
            other => Err(Error::from(format!(
                "unknown format {other:?} (xlsx, csv or ic)"
            ))),
        }
    }

    /// `sheet_exec {workbook_id, script}` → results + recalc echo.
    pub fn exec(
        &mut self,
        workbook_id: &str,
        script: &str,
        principal: &str,
        cmd_id: Option<&str>,
    ) -> Result<String> {
        // Ensure the session exists before announcing the exec.
        self.session(workbook_id)?;
        if let Some(obs) = &self.observer {
            obs.exec_started(workbook_id, principal, script);
        }
        let session = self.manager.get_mut(workbook_id)?;
        let out = cmd::exec(session, script, principal);
        let diffs = session.book.flush_diffs();
        let multi_sheet = session.book.sheet_count() > 1;

        let event = ExecEvent {
            workbook_id: workbook_id.to_string(),
            principal: principal.to_string(),
            cmd_id: cmd_id.map(String::from),
            ok: out.ok(),
            summary: out.results.join("\n"),
            delta: out
                .delta
                .changes
                .iter()
                .take(EVENT_DELTA_CAP)
                .map(|c| {
                    (
                        format!("{}!{}", c.sheet_name, c.addr()),
                        c.old.display(),
                        c.new.display(),
                    )
                })
                .collect(),
            delta_total: out.delta.len(),
            diffs,
            error: out.failed.as_ref().map(|(line, _, err)| (*line, err.clone())),
        };
        let changed = !out.delta.is_empty();
        let rendered = out.render(multi_sheet);
        if changed {
            self.persist(workbook_id);
        }
        if let Some(obs) = &self.observer {
            obs.exec_finished(&event);
        }
        Ok(rendered)
    }

    /// `sheet_view {workbook_id, target, mode?, budget_tokens?}`.
    pub fn view(
        &mut self,
        workbook_id: &str,
        target: &str,
        mode: Option<&str>,
        budget_tokens: Option<usize>,
    ) -> Result<String> {
        let session = self.session(workbook_id)?;
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
        let session = self.session(workbook_id)?;
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
    pub fn call(&mut self, name: &str, args: &Json, principal: &str) -> Result<String> {
        let s = |key: &str| args.get(key).and_then(Json::as_str);
        match name {
            "sheet_open" => self.open(s("path"), s("new")),
            "sheet_exec" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                let script = s("script").ok_or(Error::from("script is required"))?;
                self.exec(id, script, principal, None)
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
            "description": "Read a range, region, or sheet as text. Small ranges render dense (grid with formulas AND computed values); large regions render aggregated (per-column type, min..max, distinct count, fill formulas, deviations); scattered data renders as a value-grouped index. Any truncation is announced explicitly. Prefer region names over guessing A1 bounds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workbook_id": { "type": "string" },
                    "target": { "type": "string", "description": "A1 range (Sheet2!A1:C10), region name, defined name, or sheet name" },
                    "mode": { "type": "string", "enum": ["dense", "aggregated", "sparse"], "description": "Force an encoding (default: auto by size and density)" },
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
