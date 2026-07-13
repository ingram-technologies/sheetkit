//! Workbook sessions: an open [`Book`] plus everything stateful around it —
//! current sheet, named checkpoints, highlights, and the cached region index.
//! The [`Manager`] keys sessions by workbook id for the server layer.

use std::collections::HashMap;

use crate::addr::{parse_target, Range};
use crate::book::{Book, Resolved};
use crate::regions::{detect_all, Region};
use crate::{Error, Result};

/// Region list plus the name → (sheet, range) index used for resolution.
pub type RegionIndex = (Vec<Region>, HashMap<String, (u32, Range)>);

/// A range flagged for discussion, by the agent or a human.
#[derive(Debug, Clone, PartialEq)]
pub struct Highlight {
    pub id: u32,
    pub sheet: u32,
    pub sheet_name: String,
    pub range: Range,
    pub color: String,
    pub note: Option<String>,
    pub author: String,
    pub resolved: bool,
}

pub struct Session {
    pub book: Book,
    /// Sheet that unqualified references resolve against.
    pub current_sheet: u32,
    /// Where the workbook came from, if it came from a file.
    pub origin: Option<String>,
    pub highlights: Vec<Highlight>,
    next_highlight_id: u32,
    checkpoints: Vec<(String, Vec<u8>)>,
    regions_cache: Option<RegionIndex>,
}

impl Session {
    pub fn new(book: Book, origin: Option<String>) -> Session {
        Session {
            book,
            current_sheet: 0,
            origin,
            highlights: Vec::new(),
            next_highlight_id: 1,
            checkpoints: Vec::new(),
            regions_cache: None,
        }
    }

    /// Call after any mutation: region structure may have changed.
    pub fn invalidate(&mut self) {
        self.regions_cache = None;
    }

    pub fn regions(&mut self) -> &RegionIndex {
        if self.regions_cache.is_none() {
            self.regions_cache = Some(detect_all(&self.book));
        }
        self.regions_cache.as_ref().unwrap()
    }

    /// Parse and resolve a target string against this session.
    pub fn resolve(&mut self, input: &str) -> Result<Resolved> {
        let target = parse_target(input)
            .ok_or_else(|| Error::from(format!("cannot parse target {input:?}")))?;
        let current = self.current_sheet;
        self.regions(); // ensure cache
        let (_, index) = self.regions_cache.as_ref().unwrap();
        let index = index.clone();
        self.book.resolve(&target, current, &index)
    }

    // ---- checkpoints -------------------------------------------------------

    pub fn checkpoint(&mut self, name: &str) {
        let bytes = self.book.to_bytes();
        if let Some(entry) = self.checkpoints.iter_mut().find(|(n, _)| n == name) {
            entry.1 = bytes;
        } else {
            self.checkpoints.push((name.to_string(), bytes));
        }
    }

    pub fn restore(&mut self, name: &str) -> Result<()> {
        let bytes = self
            .checkpoints
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| {
                Error::from(format!(
                    "no checkpoint named {name:?} (have: {})",
                    self.checkpoint_names().join(", ")
                ))
            })?;
        self.book = Book::from_bytes(&bytes)?;
        self.invalidate();
        Ok(())
    }

    pub fn checkpoint_names(&self) -> Vec<String> {
        self.checkpoints.iter().map(|(n, _)| n.clone()).collect()
    }

    // ---- highlights ----------------------------------------------------------

    pub fn add_highlight(
        &mut self,
        sheet: u32,
        sheet_name: &str,
        range: Range,
        color: &str,
        note: Option<String>,
        author: &str,
    ) -> u32 {
        let id = self.next_highlight_id;
        self.next_highlight_id += 1;
        self.highlights.push(Highlight {
            id,
            sheet,
            sheet_name: sheet_name.to_string(),
            range,
            color: color.to_string(),
            note,
            author: author.to_string(),
            resolved: false,
        });
        id
    }

    pub fn remove_highlight(&mut self, id: u32) -> bool {
        let before = self.highlights.len();
        self.highlights.retain(|h| h.id != id);
        self.highlights.len() != before
    }
}

/// Registry of open sessions, keyed by workbook id (`wb1`, `wb2`, …).
#[derive(Default)]
pub struct Manager {
    sessions: HashMap<String, Session>,
    counter: u32,
}

impl Manager {
    pub fn new() -> Manager {
        Manager::default()
    }

    pub fn insert(&mut self, session: Session) -> String {
        self.counter += 1;
        let id = format!("wb{}", self.counter);
        self.sessions.insert(id.clone(), session);
        id
    }

    /// Register a session under a caller-chosen id (server mode uses stable
    /// random ids that survive restarts via the blob store).
    pub fn insert_with_id(&mut self, id: &str, session: Session) {
        self.sessions.insert(id.to_string(), session);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Result<&mut Session> {
        if !self.sessions.contains_key(id) {
            let open = if self.sessions.is_empty() {
                "none".to_string()
            } else {
                self.sessions.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(Error::from(format!("no open workbook {id:?} (open: {open})")));
        }
        Ok(self.sessions.get_mut(id).unwrap())
    }

    pub fn close(&mut self, id: &str) -> Result<()> {
        self.sessions
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| Error::from(format!("no open workbook {id:?}")))
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Value;

    #[test]
    fn checkpoint_and_restore() {
        let mut book = Book::new_empty("t").unwrap();
        book.set_input(0, 1, 1, "1").unwrap();
        let mut s = Session::new(book, None);
        s.checkpoint("start");
        s.book.set_input(0, 1, 1, "999").unwrap();
        assert_eq!(s.book.value(0, 1, 1), Value::Number(999.0));
        s.restore("start").unwrap();
        assert_eq!(s.book.value(0, 1, 1), Value::Number(1.0));
    }

    #[test]
    fn resolve_region_by_name() {
        let mut book = Book::new_empty("t").unwrap();
        book.batch(|b| {
            b.set_input(0, 1, 1, "H")?;
            b.set_input(0, 2, 1, "1")?;
            b.set_input(0, 3, 1, "2")?;
            Ok(())
        })
        .unwrap();
        let mut s = Session::new(book, None);
        let r = s.resolve("table1").unwrap();
        assert_eq!(r.range.a1(), "A1:A3");
        assert!(s.resolve("nope").is_err());
    }

    #[test]
    fn manager_lifecycle() {
        let mut m = Manager::new();
        let id = m.insert(Session::new(Book::new_empty("a").unwrap(), None));
        assert_eq!(id, "wb1");
        assert!(m.get_mut(&id).is_ok());
        m.close(&id).unwrap();
        assert!(m.get_mut(&id).is_err());
    }
}
