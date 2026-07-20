//! wasm-bindgen bindings over the sheetkit session: the same verbs, sketch,
//! and delta echo that sheetd serves — running in the caller's own runtime
//! (browser tab, Node, edge worker). One workbook per [`WasmSession`]; the
//! host owns persistence via engine bytes.

use wasm_bindgen::prelude::*;

use sheetkit::book::Book;
use sheetkit::cmd;
use sheetkit::session::Session;
use sheetkit::view::{self, Mode, ViewOptions};

fn err(e: sheetkit::Error) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen]
pub struct WasmSession {
    inner: Session,
}

#[wasm_bindgen]
impl WasmSession {
    /// Fresh empty workbook.
    #[wasm_bindgen(constructor)]
    pub fn new(name: &str) -> Result<WasmSession, JsError> {
        let book = Book::new_empty(name).map_err(err)?;
        Ok(WasmSession {
            inner: Session::new(book, None),
        })
    }

    /// Open from IronCalc engine bytes (the persistence format).
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<WasmSession, JsError> {
        let book = Book::from_bytes(bytes).map_err(err)?;
        Ok(WasmSession {
            inner: Session::new(book, None),
        })
    }

    /// Open from CSV text.
    #[wasm_bindgen(js_name = fromCsv)]
    pub fn from_csv(csv: &str, name: &str) -> Result<WasmSession, JsError> {
        let book = Book::from_csv_str(csv, name).map_err(err)?;
        Ok(WasmSession {
            inner: Session::new(book, None),
        })
    }

    /// Serialize to engine bytes for the host to persist.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.book.to_bytes()
    }

    /// The structure-aware workbook sketch (regions, headers, column types).
    pub fn sketch(&mut self) -> String {
        let (regions, _) = self.inner.regions().clone();
        view::sketch(&self.inner.book, &regions)
    }

    /// Run a command script (`set`, `fill`, `sort`, `expect`, …); returns the
    /// rendered outcome including the recalc delta echo.
    pub fn exec(&mut self, script: &str, author: &str) -> String {
        let out = cmd::exec(&mut self.inner, script, author);
        let multi_sheet = self.inner.book.sheet_count() > 1;
        out.render(multi_sheet)
    }

    /// Render a range/table view under a token budget. `mode` is "dense",
    /// "sparse", "agg", or empty for auto.
    pub fn view(
        &mut self,
        target: &str,
        mode: &str,
        budget_tokens: usize,
    ) -> Result<String, JsError> {
        let mut opts = ViewOptions::default();
        if budget_tokens > 0 {
            opts.budget_tokens = budget_tokens;
        }
        opts.mode = match mode {
            "" | "auto" => None,
            "dense" => Some(Mode::Dense),
            "sparse" => Some(Mode::Sparse),
            "agg" | "aggregated" => Some(Mode::Aggregated),
            other => return Err(JsError::new(&format!("unknown mode {other:?}"))),
        };
        let resolved = self.inner.resolve(target).map_err(err)?;
        self.inner.current_sheet = resolved.sheet_index;
        let (regions, _) = self.inner.regions().clone();
        Ok(view::render_view(
            &self.inner.book,
            &resolved,
            &regions,
            opts,
        ))
    }

    /// Export the current sheet as CSV.
    #[wasm_bindgen(js_name = toCsv)]
    pub fn to_csv(&self) -> Result<String, JsError> {
        self.inner
            .book
            .to_csv(self.inner.current_sheet)
            .map_err(err)
    }
}
