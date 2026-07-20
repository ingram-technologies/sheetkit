//! The workbook tools, shared by every transport (stdio MCP, HTTP MCP, REST,
//! WebSocket). Thin orchestration over `sheetkit`: sessions in, rendered text
//! out, with optional blob persistence and an observer for realtime fan-out.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use sheetkit::book::Book;
use sheetkit::cmd;
use sheetkit::session::{Manager, Session};
use sheetkit::view::{self, Mode, ViewOptions};
use sheetkit::{Error, Result};

use crate::store::{JournalHistory, Store};

/// What one script run did — everything a realtime channel needs to fan out.
pub struct ExecEvent {
    pub workbook_id: String,
    pub principal: String,
    pub cmd_id: Option<String>,
    pub ok: bool,
    /// Monotonic per-workbook sequence, assigned when the exec changed state
    /// (0 for read-only execs, which fan nothing out).
    pub seq: u64,
    /// Per-command result lines (already human/model readable).
    pub summary: String,
    /// Changed cells, capped at [`EVENT_DELTA_CAP`]: `(addr, old, new)`.
    pub delta: Vec<(String, String, String)>,
    pub delta_total: usize,
    /// The engine's opaque diff blob for same-version replicas (may be empty).
    pub diffs: Vec<u8>,
    /// The book was swapped wholesale (restore, journal undo, file replace):
    /// the diff blob cannot express it — replicas must refetch.
    pub resync: bool,
    /// `(line number, message)` when the script stopped early.
    pub error: Option<(usize, String)>,
}

pub const EVENT_DELTA_CAP: usize = 200;

/// Observer for realtime transports; no-op default methods keep stdio simple.
pub trait ExecObserver: Send {
    fn exec_started(&self, _workbook_id: &str, _principal: &str, _script: &str) {}
    fn exec_finished(&self, _event: &ExecEvent) {}
}

/// The `applied` frame for an exec — the same JSON goes to the journal and
/// onto the channel, so replay resends exact bytes.
pub fn applied_frame(e: &ExecEvent) -> Json {
    use base64::Engine as _;
    json!({
        "type": "applied",
        "seq": e.seq,
        "principal": e.principal,
        "cmd_id": e.cmd_id,
        "ok": e.ok,
        "summary": e.summary,
        "delta": e.delta.iter().map(|(a, o, n)| json!([a, o, n])).collect::<Vec<Json>>(),
        "delta_total": e.delta_total,
        "diffs_b64": base64::engine::general_purpose::STANDARD.encode(&e.diffs),
        "resync": e.resync,
    })
}

pub struct Tools {
    pub manager: Manager,
    /// Blob store: source of truth in server mode. Sessions are a cache over
    /// it — persisted on every mutation, rehydrated on demand, evictable.
    pub store: Option<Store>,
    store_dir: Option<PathBuf>,
    /// Server mode uses stable random ids instead of the wb1/wb2 counter.
    pub random_ids: bool,
    pub observer: Option<Box<dyn ExecObserver>>,
    /// Reject imports with more non-empty cells than this (0 = unlimited).
    pub max_cells: u64,
    /// Evict least-recently-used sessions past this count (0 = unlimited).
    pub max_resident: usize,
    last_access: HashMap<String, Instant>,
    /// Sequence counters for workbooks without a store (ephemeral mode).
    mem_seq: HashMap<String, u64>,
    id_salt: u64,
}

impl Tools {
    pub fn new() -> Tools {
        Tools {
            manager: Manager::new(),
            store: None,
            store_dir: None,
            random_ids: false,
            observer: None,
            max_cells: 0,
            max_resident: 0,
            last_access: HashMap::new(),
            mem_seq: HashMap::new(),
            id_salt: 0,
        }
    }

    pub fn with_store(dir: PathBuf) -> Tools {
        let mut t = Tools::new();
        t.store = Some(Store::new(dir.clone()));
        t.store_dir = Some(dir);
        t
    }

    pub fn current_seq(&self, id: &str) -> u64 {
        match &self.store {
            Some(store) => store.current_seq(id),
            None => self.mem_seq.get(id).copied().unwrap_or(0),
        }
    }

    fn next_seq(&mut self, id: &str) -> u64 {
        let n = self.current_seq(id) + 1;
        if self.store.is_none() {
            self.mem_seq.insert(id.to_string(), n);
        }
        n
    }

    /// Journal frames with `seq > after` (channel replay).
    pub fn frames_after(&self, id: &str, after: u64) -> Vec<Json> {
        self.store
            .as_ref()
            .map(|s| s.frames_after(id, after))
            .unwrap_or_default()
    }

    fn touch(&mut self, id: &str) {
        self.last_access.insert(id.to_string(), Instant::now());
    }

    /// Evict least-recently-used sessions above `max_resident`, never the one
    /// named `keep`. State is already persisted eagerly, so eviction is a drop.
    fn enforce_cap(&mut self, keep: &str) {
        if self.max_resident == 0 || self.store.is_none() {
            return;
        }
        while self.manager.len() > self.max_resident {
            let Some(victim) = self
                .last_access
                .iter()
                .filter(|(id, _)| id.as_str() != keep && self.manager.contains(id))
                .min_by_key(|(_, t)| **t)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.manager.remove(&victim);
            self.last_access.remove(&victim);
        }
    }

    /// Drop sessions idle for longer than `ttl`; returns the evicted ids.
    /// Only meaningful with a store (nothing would survive otherwise).
    pub fn evict_idle(&mut self, ttl: Duration) -> Vec<String> {
        if self.store.is_none() {
            return vec![];
        }
        let now = Instant::now();
        let victims: Vec<String> = self
            .last_access
            .iter()
            .filter(|(id, t)| self.manager.contains(id) && now.duration_since(**t) > ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &victims {
            self.manager.remove(id);
            self.last_access.remove(id);
        }
        victims
    }

    /// Admission control: a workbook this large would be a memory hazard.
    fn admit(&self, book: &Book) -> Result<()> {
        if self.max_cells > 0 {
            let cells = book.non_empty_count();
            if cells > self.max_cells {
                return Err(Error::from(format!(
                    "workbook has {cells} non-empty cells, over this server's limit of {} — split the file or raise --max-cells",
                    self.max_cells
                )));
            }
        }
        Ok(())
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

    fn insert(&mut self, mut session: Session) -> String {
        // Drop diffs accumulated while building the book (csv/gsheets import):
        // replicas bootstrap from the persisted state, so replaying import
        // diffs on top would double-apply.
        let _ = session.book.flush_diffs();
        let id = if self.random_ids {
            let id = self.new_id();
            self.manager.insert_with_id(&id, session);
            id
        } else {
            self.manager.insert(session)
        };
        self.install_history(&id);
        self.persist(&id, 0, true);
        self.touch(&id);
        self.enforce_cap(&id);
        id
    }

    fn install_history(&mut self, id: &str) {
        let Some(dir) = self.store_dir.clone() else {
            return;
        };
        if let Ok(session) = self.manager.get_mut(id) {
            session.history = Some(Box::new(JournalHistory {
                dir,
                id: id.to_string(),
            }));
        }
    }

    /// Get a session, rehydrating from the blob store when needed.
    pub fn session(&mut self, id: &str) -> Result<&mut Session> {
        if !self.manager.contains(id) {
            if let Some(store) = &self.store {
                if let Some(session) = store.load_session(id)? {
                    self.manager.insert_with_id(id, session);
                    self.install_history(id);
                    self.enforce_cap(id);
                }
            }
        }
        self.touch(id);
        self.manager.get_mut(id)
    }

    /// Best-effort persistence of head + meta + ephemera at `seq`;
    /// failures are logged, not fatal. `reset_base` breaks the journal chain
    /// (whole-book swaps) so the tail always replays cleanly.
    fn persist(&mut self, id: &str, seq: u64, reset_base: bool) {
        let Some(store) = &self.store else {
            return;
        };
        let Ok(session) = self.manager.get_mut(id) else {
            return;
        };
        if let Err(e) = store.save_session(id, session, seq, reset_base) {
            eprintln!("sheetd: failed to persist {id}: {e}");
        }
    }

    fn sketch_text(session: &mut Session) -> String {
        let (regions, _) = session.regions().clone();
        view::sketch(&session.book, &regions)
    }

    // ---- the five tools -----------------------------------------------------

    /// `sheet_open {path | new}` → workbook id + structure sketch. `path`
    /// also accepts a Google Sheets URL or `gsheets:<id>` ref (pull).
    pub fn open(&mut self, path: Option<&str>, new: Option<&str>) -> Result<String> {
        if let (Some(p), None) = (path, new) {
            if let Some(sid) = sheetkit::gsheets::parse_spreadsheet_id(p) {
                return self.open_gsheets(&sid);
            }
        }
        let (book, origin) = match (path, new) {
            (Some(p), None) => (Book::open(p)?, Some(p.to_string())),
            (None, name) => (Book::new_empty(name.unwrap_or("workbook"))?, None),
            (Some(_), Some(_)) => return Err(Error::from("pass either `path` or `new`, not both")),
        };
        self.admit(&book)?;
        let mut session = Session::new(book, origin.clone());
        let sketch = Self::sketch_text(&mut session);
        let id = self.insert(session);
        let origin_note = origin.map(|p| format!(" from {p}")).unwrap_or_default();
        Ok(format!("opened {id}{origin_note}\n\n{sketch}"))
    }

    /// Pull a Google spreadsheet: one `spreadsheets.get`, then everything is
    /// local until `sheet_save` pushes the content diff back.
    fn open_gsheets(&mut self, spreadsheet_id: &str) -> Result<String> {
        let response = crate::gs::fetch_spreadsheet(spreadsheet_id)?;
        let imp = sheetkit::gsheets::import(spreadsheet_id, &response)?;
        self.admit(&imp.book)?;
        let mut session = Session::new(imp.book, Some(format!("gsheets:{spreadsheet_id}")));
        session.gsheets = Some(imp.baseline);
        let sketch = Self::sketch_text(&mut session);
        let id = self.insert(session);
        let mut out = format!(
            "opened {id} from Google Sheets {spreadsheet_id} (local copy; `sheet_save` pushes changes back)\n"
        );
        for w in &imp.warnings {
            out.push_str(&format!("⚠ {w}\n"));
        }
        out.push('\n');
        out.push_str(&sketch);
        Ok(out)
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
        self.admit(&book)?;
        Ok(self.insert(Session::new(book, None)))
    }

    /// Replace an open workbook's content in place, keeping its id. This is
    /// a whole-book swap: replicas get a `resync` frame.
    pub fn replace_bytes(
        &mut self,
        id: &str,
        format: &str,
        bytes: &[u8],
        principal: &str,
    ) -> Result<()> {
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
        self.admit(&book)?;
        let session = self.session(id)?;
        session.book = book;
        session.invalidate();
        let _ = session.book.flush_diffs();
        let seq = self.next_seq(id);
        let event = ExecEvent {
            workbook_id: id.to_string(),
            principal: principal.to_string(),
            cmd_id: None,
            ok: true,
            seq,
            summary: format!("workbook file replaced ({format})"),
            delta: vec![],
            delta_total: 0,
            diffs: vec![],
            resync: true,
            error: None,
        };
        self.persist(id, seq, true);
        self.append_journal(&event);
        if let Some(obs) = &self.observer {
            obs.exec_finished(&event);
        }
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
        let ephemera_before = (session.checkpoint_names(), session.highlights.len());
        let out = cmd::exec(session, script, principal);
        let diffs = session.book.flush_diffs();
        let multi_sheet = session.book.sheet_count() > 1;
        let ephemera_changed =
            ephemera_before != (session.checkpoint_names(), session.highlights.len());

        // A state change gets a sequence number and a journal entry;
        // read-only execs get neither.
        let changed = !out.delta.is_empty() || out.needs_resync;
        let seq = if changed {
            self.next_seq(workbook_id)
        } else {
            0
        };

        let event = ExecEvent {
            workbook_id: workbook_id.to_string(),
            principal: principal.to_string(),
            cmd_id: cmd_id.map(String::from),
            ok: out.ok(),
            seq,
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
            resync: out.needs_resync,
            error: out
                .failed
                .as_ref()
                .map(|(line, _, err)| (*line, err.clone())),
        };
        let rendered = out.render(multi_sheet);
        if changed {
            // Persist first: a resync transition resets the base and wipes
            // the old journal tail, and this frame must land after that.
            self.persist(workbook_id, seq, event.resync);
            self.append_journal(&event);
        } else if ephemera_changed {
            self.persist(workbook_id, self.current_seq(workbook_id), false);
        }
        if let Some(obs) = &self.observer {
            obs.exec_finished(&event);
        }
        Ok(rendered)
    }

    /// Append an applied frame to the journal (exact bytes the channel sees).
    fn append_journal(&self, event: &ExecEvent) {
        let Some(store) = &self.store else {
            return;
        };
        let frame = applied_frame(event);
        if let Err(e) = store.append_frame(&event.workbook_id, &frame) {
            eprintln!(
                "sheetd: journal append failed for {}: {e}",
                event.workbook_id
            );
        }
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
    /// to where the workbook was opened from — a file write, or a Google
    /// Sheets push when the workbook was pulled from one.
    pub fn save(
        &mut self,
        workbook_id: &str,
        path: Option<&str>,
        overwrite: bool,
    ) -> Result<String> {
        let session = self.session(workbook_id)?;
        let origin_is_gsheets = path.is_none()
            && session
                .origin
                .as_deref()
                .is_some_and(|o| o.starts_with("gsheets:"));
        if origin_is_gsheets {
            return self.save_gsheets(workbook_id);
        }
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

    /// Push the content diff back to the origin spreadsheet: a structural
    /// drift tripwire (light properties fetch), then one `batchUpdate`.
    fn save_gsheets(&mut self, workbook_id: &str) -> Result<String> {
        let session = self.manager.get_mut(workbook_id)?;
        let sid = session
            .origin
            .as_deref()
            .and_then(|o| o.strip_prefix("gsheets:"))
            .map(String::from)
            .ok_or_else(|| Error::from("not a Google Sheets workbook"))?;
        let Some(baseline) = session.gsheets.clone() else {
            return Err(Error::from(
                "this workbook lost its push baseline (e.g. it was rehydrated from the blob store); re-open the spreadsheet URL to pull a fresh baseline, then re-apply your changes",
            ));
        };
        let push = sheetkit::gsheets::push_requests(&session.book, &baseline)?;
        if push.requests.is_empty() {
            return Ok(format!("no changes to push to Google Sheets {sid}"));
        }
        let mut warnings = push.warnings.clone();
        match crate::gs::fetch_sheet_properties(&sid) {
            Ok(props) => {
                if let Some(w) = sheetkit::gsheets::structural_drift(&baseline, &props) {
                    warnings.push(w);
                }
            }
            Err(e) => warnings.push(format!("could not check for remote drift: {e}")),
        }
        crate::gs::push_batch(&sid, &push.requests)?;
        if let Some(b) = session.gsheets.as_mut() {
            sheetkit::gsheets::apply_push_to_baseline(b, &push.requests, &session.book);
        }
        // Persist the fast-forwarded baseline so a later rehydration can push.
        let seq = self.current_seq(workbook_id);
        self.persist(workbook_id, seq, false);
        let mut out = format!(
            "pushed {} changed cell{} ({} request{}) to Google Sheets {sid}",
            push.changed_cells,
            if push.changed_cells == 1 { "" } else { "s" },
            push.requests.len(),
            if push.requests.len() == 1 { "" } else { "s" },
        );
        for w in warnings {
            out.push_str(&format!("\n⚠ {w}"));
        }
        Ok(out)
    }

    /// `sheet_close {workbook_id}`. The persisted blobs stay (rehydration);
    /// `purge` removes them too.
    pub fn close(&mut self, workbook_id: &str) -> Result<String> {
        self.manager.close(workbook_id)?;
        self.last_access.remove(workbook_id);
        Ok(format!("closed {workbook_id}"))
    }

    pub fn purge(&mut self, workbook_id: &str) -> Result<String> {
        let existed = self.manager.remove(workbook_id)
            || self.store.as_ref().is_some_and(|s| s.exists(workbook_id));
        self.last_access.remove(workbook_id);
        if let Some(store) = &self.store {
            store.delete(workbook_id, true)?;
        }
        if !existed {
            return Err(Error::from(format!("no workbook {workbook_id:?}")));
        }
        Ok(format!("purged {workbook_id}"))
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
                let budget = args
                    .get("budget_tokens")
                    .and_then(Json::as_u64)
                    .map(|b| b as usize);
                self.view(id, target, s("mode"), budget)
            }
            "sheet_save" => {
                let id = s("workbook_id").ok_or(Error::from("workbook_id is required"))?;
                let overwrite = args
                    .get("overwrite")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
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
            "description": "Open a spreadsheet (xlsx, csv, or ic file — or a Google Sheets URL / gsheets:<id>, pulled once and worked on locally; requires GSHEETS_TOKEN) or create a new one. Returns a workbook_id plus a structure sketch: every sheet, detected table regions with per-column types/ranges/fill formulas, and defined names. Read the sketch instead of dumping cells — it is usually all you need to start working.",
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
            "description": "Save the workbook. Without a path, saves back to where it was opened from: a file write, or — for workbooks opened from a Google Sheets URL — a single batched push of every changed cell (last-write-wins; structural remote drift is detected and warned about). With a path, format follows the extension: .xlsx, .csv, .ic. Saving to a NEW existing path requires overwrite=true.",
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
