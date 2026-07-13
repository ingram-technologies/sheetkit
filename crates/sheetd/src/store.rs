//! The workbook store: blobs as the source of truth, sessions as a cache.
//!
//! Per workbook id, the store keeps:
//!
//! - `{id}.ic` — the **head** blob (current state), rewritten on every mutation
//! - `{id}.base.ic` — the **journal base**: the state the journal tail applies on
//! - `{id}.journal.jsonl` — the exec **journal**: one `applied` frame per line,
//!   exactly as broadcast on the channel (seq, principal, summary, delta,
//!   `diffs_b64`, `resync`). It serves three masters: undo for rehydrated
//!   sessions (replay base + tail), channel replay after reconnect, and audit.
//! - `{id}.meta.json` — sequence counters plus session ephemera (current
//!   sheet, highlights, checkpoint names)
//! - `{id}.ckpt-{hash}.ic` — named checkpoint blobs
//! - `{id}.gsheets.json` — the Google Sheets push baseline, when pulled
//!
//! Frames whose transition the engine diff queue cannot express (checkpoint
//! restore, journal undo, file replace) carry `resync: true`; appending one
//! also rewrites the base, so the journal tail always replays cleanly.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value as Json};
use sheetkit::book::Book;
use sheetkit::session::{Highlight, Session};
use sheetkit::{Error, Result};

/// Rewrite the base once the journal tail grows past twice this many entries,
/// keeping this many for undo/replay.
const COMPACT_KEEP: usize = 24;

pub struct Store {
    dir: PathBuf,
}

/// Everything `meta.json` carries besides the raw workbook bytes.
pub struct Meta {
    pub seq: u64,
    pub base_seq: u64,
    pub current_sheet: u32,
    pub next_highlight_id: u32,
    pub highlights: Vec<Highlight>,
    pub checkpoints: Vec<String>,
    pub origin: Option<String>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn head_path(&self, id: &str) -> PathBuf {
        self.path(&format!("{id}.ic"))
    }

    fn base_path(&self, id: &str) -> PathBuf {
        self.path(&format!("{id}.base.ic"))
    }

    fn journal_path(&self, id: &str) -> PathBuf {
        self.path(&format!("{id}.journal.jsonl"))
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.path(&format!("{id}.meta.json"))
    }

    fn ckpt_path(&self, id: &str, name: &str) -> PathBuf {
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        self.path(&format!("{id}.ckpt-{:08x}.ic", (h.finish() & 0xffff_ffff) as u32))
    }

    fn gsheets_path(&self, id: &str) -> PathBuf {
        self.path(&format!("{id}.gsheets.json"))
    }

    pub fn exists(&self, id: &str) -> bool {
        self.head_path(id).exists()
    }

    // ---- persistence --------------------------------------------------------

    /// Write head blob + meta + ephemera for a session. The base is only
    /// (re)written when absent or when `reset_base` demands a chain break.
    pub fn save_session(&self, id: &str, session: &Session, seq: u64, reset_base: bool) -> Result<()> {
        let head = session.book.to_bytes();
        write_atomic(&self.head_path(id), &head)?;
        let mut base_seq = self.load_meta(id).map(|m| m.base_seq).unwrap_or(0);
        if reset_base || !self.base_path(id).exists() {
            write_atomic(&self.base_path(id), &head)?;
            base_seq = seq;
            // The old journal tail no longer applies on this base.
            let _ = std::fs::remove_file(self.journal_path(id));
        }
        // Checkpoint blobs: write new ones, drop removed ones.
        let old_names: Vec<String> = self.load_meta(id).map(|m| m.checkpoints).unwrap_or_default();
        let names: Vec<String> = session.checkpoint_names();
        for (name, bytes) in session.checkpoints_raw() {
            let p = self.ckpt_path(id, name);
            if !p.exists() {
                write_atomic(&p, bytes)?;
            }
        }
        for gone in old_names.iter().filter(|n| !names.contains(n)) {
            let _ = std::fs::remove_file(self.ckpt_path(id, gone));
        }
        self.save_meta(
            id,
            &Meta {
                seq,
                base_seq,
                current_sheet: session.current_sheet,
                next_highlight_id: session.next_highlight_id(),
                highlights: session.highlights.clone(),
                checkpoints: names,
                origin: session.origin.clone(),
            },
        )?;
        if let Some(baseline) = &session.gsheets {
            self.save_baseline(id, baseline)?;
        }
        Ok(())
    }

    /// Load a full session (book + ephemera). `None` when the id is unknown.
    pub fn load_session(&self, id: &str) -> Result<Option<Session>> {
        let Ok(bytes) = std::fs::read(self.head_path(id)) else {
            return Ok(None);
        };
        let book = Book::from_bytes(&bytes)?;
        let meta = self.load_meta(id);
        let mut session = Session::new(book, meta.as_ref().and_then(|m| m.origin.clone()));
        if let Some(meta) = meta {
            session.current_sheet = meta.current_sheet;
            session.highlights = meta.highlights;
            session.set_next_highlight_id(meta.next_highlight_id);
            let mut checkpoints = Vec::new();
            for name in meta.checkpoints {
                if let Ok(blob) = std::fs::read(self.ckpt_path(id, &name)) {
                    checkpoints.push((name, blob));
                }
            }
            session.set_checkpoints(checkpoints);
        }
        session.gsheets = self.load_baseline(id)?;
        Ok(Some(session))
    }

    // ---- the journal ---------------------------------------------------------

    /// Current sequence number (0 when nothing was ever journaled).
    pub fn current_seq(&self, id: &str) -> u64 {
        self.load_meta(id).map(|m| m.seq).unwrap_or(0)
    }

    /// Append one applied frame; the caller must have set `frame["seq"]` to
    /// `current_seq + 1` (use [`Store::save_session`] afterwards to persist
    /// the matching head + seq). Compacts the base when the tail grows long.
    pub fn append_frame(&self, id: &str, frame: &Json) -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path(id))
            .map_err(|e| Error::from(format!("journal append failed: {e}")))?;
        let mut line = frame.to_string();
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| Error::from(format!("journal append failed: {e}")))?;
        drop(file);
        self.maybe_compact(id)?;
        Ok(())
    }

    /// Frames with `seq > after`, in order.
    pub fn frames_after(&self, id: &str, after: u64) -> Vec<Json> {
        let Ok(text) = std::fs::read_to_string(self.journal_path(id)) else {
            return vec![];
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<Json>(l).ok())
            .filter(|f| f.get("seq").and_then(Json::as_u64).unwrap_or(0) > after)
            .collect()
    }

    /// Reconstruct the workbook as it was at `seq`: base blob + journal tail.
    /// `None` when `seq` predates the base (compacted away or chain-broken).
    pub fn state_at(&self, id: &str, seq: u64) -> Result<Option<Book>> {
        let Some(meta) = self.load_meta(id) else {
            return Ok(None);
        };
        if seq < meta.base_seq || seq > meta.seq {
            return Ok(None);
        }
        let Ok(base) = std::fs::read(self.base_path(id)) else {
            return Ok(None);
        };
        let mut book = Book::from_bytes(&base)?;
        for frame in self.frames_after(id, meta.base_seq) {
            let fseq = frame.get("seq").and_then(Json::as_u64).unwrap_or(0);
            if fseq > seq {
                break;
            }
            if frame.get("resync").and_then(Json::as_bool).unwrap_or(false) {
                // Chain break inside the window — cannot replay across it.
                return Ok(None);
            }
            let b64 = frame.get("diffs_b64").and_then(Json::as_str).unwrap_or("");
            if !b64.is_empty() {
                use base64::Engine as _;
                let blob = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| Error::from(format!("corrupt journal entry {fseq}: {e}")))?;
                book.apply_diffs(&blob)?;
            }
        }
        book.evaluate();
        Ok(Some(book))
    }

    /// Move the base forward once the tail exceeds 2×[`COMPACT_KEEP`],
    /// keeping [`COMPACT_KEEP`] entries for undo/replay.
    fn maybe_compact(&self, id: &str) -> Result<()> {
        let Some(meta) = self.load_meta(id) else {
            return Ok(());
        };
        let frames = self.frames_after(id, meta.base_seq);
        if frames.len() <= COMPACT_KEEP * 2 {
            return Ok(());
        }
        let cut = frames.len() - COMPACT_KEEP;
        let new_base_seq = frames[cut - 1]
            .get("seq")
            .and_then(Json::as_u64)
            .unwrap_or(meta.base_seq);
        let Some(book) = self.state_at(id, new_base_seq)? else {
            return Ok(()); // resync frame inside the window; base resets on its own
        };
        write_atomic(&self.base_path(id), &book.to_bytes())?;
        let tail: Vec<String> = frames[cut..].iter().map(|f| f.to_string()).collect();
        write_atomic(
            &self.journal_path(id),
            format!("{}\n", tail.join("\n")).as_bytes(),
        )?;
        let mut m = meta;
        m.base_seq = new_base_seq;
        self.save_meta(id, &m)?;
        Ok(())
    }

    // ---- meta / baseline -------------------------------------------------------

    fn save_meta(&self, id: &str, meta: &Meta) -> Result<()> {
        let highlights: Vec<Json> = meta
            .highlights
            .iter()
            .map(|h| {
                json!({
                    "id": h.id, "sheet": h.sheet, "sheet_name": h.sheet_name,
                    "range": h.range.a1(), "color": h.color, "note": h.note,
                    "author": h.author, "resolved": h.resolved,
                })
            })
            .collect();
        let doc = json!({
            "seq": meta.seq,
            "base_seq": meta.base_seq,
            "current_sheet": meta.current_sheet,
            "next_highlight_id": meta.next_highlight_id,
            "highlights": highlights,
            "checkpoints": meta.checkpoints,
            "origin": meta.origin,
            "engine": crate::serve::ENGINE_VERSION,
        });
        write_atomic(&self.meta_path(id), doc.to_string().as_bytes())
    }

    pub fn load_meta(&self, id: &str) -> Option<Meta> {
        let text = std::fs::read_to_string(self.meta_path(id)).ok()?;
        let doc: Json = serde_json::from_str(&text).ok()?;
        let highlights = doc["highlights"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|h| {
                        let target = sheetkit::addr::parse_target(h["range"].as_str()?)?;
                        let sheetkit::addr::TargetKind::Range(range) = target.kind else {
                            return None;
                        };
                        Some(Highlight {
                            id: h["id"].as_u64()? as u32,
                            sheet: h["sheet"].as_u64()? as u32,
                            sheet_name: h["sheet_name"].as_str()?.to_string(),
                            range,
                            color: h["color"].as_str().unwrap_or("amber").to_string(),
                            note: h["note"].as_str().map(String::from),
                            author: h["author"].as_str().unwrap_or("?").to_string(),
                            resolved: h["resolved"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Meta {
            seq: doc["seq"].as_u64().unwrap_or(0),
            base_seq: doc["base_seq"].as_u64().unwrap_or(0),
            current_sheet: doc["current_sheet"].as_u64().unwrap_or(0) as u32,
            next_highlight_id: doc["next_highlight_id"].as_u64().unwrap_or(1) as u32,
            highlights,
            checkpoints: doc["checkpoints"]
                .as_array()
                .map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            origin: doc["origin"].as_str().map(String::from),
        })
    }

    fn save_baseline(&self, id: &str, baseline: &sheetkit::gsheets::Baseline) -> Result<()> {
        let sheets: Vec<Json> = baseline
            .sheets
            .iter()
            .map(|s| {
                json!({
                    "sheet_id": s.sheet_id, "title": s.title,
                    "row_count": s.row_count, "column_count": s.column_count,
                })
            })
            .collect();
        let contents: Vec<Json> = baseline
            .contents
            .iter()
            .map(|((sheet, row, col), content)| json!([sheet, row, col, content]))
            .collect();
        let doc = json!({
            "spreadsheet_id": baseline.spreadsheet_id,
            "title": baseline.title,
            "sheets": sheets,
            "contents": contents,
        });
        write_atomic(&self.gsheets_path(id), doc.to_string().as_bytes())
    }

    fn load_baseline(&self, id: &str) -> Result<Option<sheetkit::gsheets::Baseline>> {
        let Ok(text) = std::fs::read_to_string(self.gsheets_path(id)) else {
            return Ok(None);
        };
        let doc: Json = serde_json::from_str(&text)
            .map_err(|e| Error::from(format!("corrupt gsheets baseline for {id}: {e}")))?;
        let mut baseline = sheetkit::gsheets::Baseline {
            spreadsheet_id: doc["spreadsheet_id"].as_str().unwrap_or_default().to_string(),
            title: doc["title"].as_str().unwrap_or_default().to_string(),
            sheets: Vec::new(),
            contents: Default::default(),
        };
        for s in doc["sheets"].as_array().into_iter().flatten() {
            baseline.sheets.push(sheetkit::gsheets::SheetMeta {
                sheet_id: s["sheet_id"].as_i64().unwrap_or(0),
                title: s["title"].as_str().unwrap_or_default().to_string(),
                row_count: s["row_count"].as_i64().unwrap_or(0) as i32,
                column_count: s["column_count"].as_i64().unwrap_or(0) as i32,
            });
        }
        for entry in doc["contents"].as_array().into_iter().flatten() {
            if let (Some(sheet), Some(row), Some(col), Some(content)) = (
                entry[0].as_str(),
                entry[1].as_i64(),
                entry[2].as_i64(),
                entry[3].as_str(),
            ) {
                baseline
                    .contents
                    .insert((sheet.to_string(), row as i32, col as i32), content.to_string());
            }
        }
        Ok(Some(baseline))
    }

    // ---- lifecycle ----------------------------------------------------------------

    /// Remove a workbook's files. Without `purge`, blobs stay for later
    /// rehydration and only matter to GC.
    pub fn delete(&self, id: &str, purge: bool) -> Result<()> {
        if !purge {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.dir)
            .map_err(|e| Error::from(format!("cannot read store dir: {e}")))?
            .flatten()
        {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("{id}.")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Delete every workbook whose newest file is older than `max_age`,
    /// skipping ids in `resident`. Returns the ids removed.
    pub fn gc(&self, max_age: std::time::Duration, resident: &[String]) -> Vec<String> {
        let mut newest: std::collections::HashMap<String, SystemTime> = Default::default();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            let Some(id) = name.split('.').next().map(String::from) else {
                continue;
            };
            if !id.starts_with("wb") {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let e = newest.entry(id).or_insert(SystemTime::UNIX_EPOCH);
            if mtime > *e {
                *e = mtime;
            }
        }
        let now = SystemTime::now();
        let mut removed = Vec::new();
        for (id, mtime) in newest {
            if resident.contains(&id) {
                continue;
            }
            if now.duration_since(mtime).map(|age| age > max_age).unwrap_or(false) {
                let _ = self.delete(&id, true);
                removed.push(id);
            }
        }
        removed
    }
}

/// Write via a temp file + rename so readers never see a torn file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| Error::from(format!("write {} failed: {e}", path.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::from(format!("rename {} failed: {e}", path.display())))
}

/// [`sheetkit::session::HistoryProvider`] backed by the journal: reads the
/// store files directly (safe: all mutations hold the global tools lock).
pub struct JournalHistory {
    pub dir: PathBuf,
    pub id: String,
}

impl sheetkit::session::HistoryProvider for JournalHistory {
    fn state_back(&self, steps: u32) -> Result<Option<Book>> {
        let store = Store::new(self.dir.clone());
        let seq = store.current_seq(&self.id);
        let Some(target) = seq.checked_sub(steps as u64) else {
            return Ok(None);
        };
        store.state_at(&self.id, target)
    }
}
