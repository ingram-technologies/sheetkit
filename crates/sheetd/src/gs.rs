//! HTTP transport for the Google Sheets adapter. The conversion logic lives
//! in `sheetkit::gsheets` (pure, tested offline); this module only moves
//! JSON over the wire.
//!
//! Configuration is environment-only, deliberately: tokens never appear in
//! tool arguments or transcripts.
//! - `GSHEETS_TOKEN` — OAuth2 access token with a spreadsheets scope; the
//!   embedding platform owns acquisition and refresh.
//! - `GSHEETS_API_BASE` — override for tests/proxies
//!   (default `https://sheets.googleapis.com`).

use serde_json::Value as Json;
use sheetkit::{Error, Result};

fn api_base() -> String {
    std::env::var("GSHEETS_API_BASE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://sheets.googleapis.com".to_string())
}

fn token() -> Result<String> {
    std::env::var("GSHEETS_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::from(
                "GSHEETS_TOKEN is not set; export an OAuth2 access token with a spreadsheets scope to work with Google Sheets",
            )
        })
}

/// The fields we pull: entered values (formulas + literals) and number
/// format types/patterns for date detection. Nothing else crosses the wire.
const PULL_FIELDS: &str = "spreadsheetId,properties.title,sheets(properties(sheetId,title,gridProperties(rowCount,columnCount)),data(startRow,startColumn,rowData(values(userEnteredValue,effectiveFormat(numberFormat(type,pattern))))))";

const PROPS_FIELDS: &str = "sheets(properties(sheetId,title,gridProperties(rowCount,columnCount)))";

fn get(url: &str, token: &str) -> Result<Json> {
    let response = ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(describe_http_error)?;
    response
        .into_json()
        .map_err(|e| Error::from(format!("bad JSON from Google Sheets API: {e}")))
}

/// Full pull: one `spreadsheets.get` with grid data.
pub fn fetch_spreadsheet(spreadsheet_id: &str) -> Result<Json> {
    let token = token()?;
    get(
        &format!(
            "{}/v4/spreadsheets/{spreadsheet_id}?includeGridData=true&fields={PULL_FIELDS}",
            api_base()
        ),
        &token,
    )
}

/// Light structural fetch (no grid data) for the pre-push drift tripwire.
pub fn fetch_sheet_properties(spreadsheet_id: &str) -> Result<Json> {
    let token = token()?;
    get(
        &format!("{}/v4/spreadsheets/{spreadsheet_id}?fields={PROPS_FIELDS}", api_base()),
        &token,
    )
}

/// Apply a batch of requests: one `spreadsheets.batchUpdate`.
pub fn push_batch(spreadsheet_id: &str, requests: &[Json]) -> Result<Json> {
    let token = token()?;
    let url = format!("{}/v4/spreadsheets/{spreadsheet_id}:batchUpdate", api_base());
    let response = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "requests": requests }))
        .map_err(describe_http_error)?;
    response
        .into_json()
        .map_err(|e| Error::from(format!("bad JSON from Google Sheets API: {e}")))
}

fn describe_http_error(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<Json>(&body)
                .ok()
                .and_then(|j| j.pointer("/error/message").and_then(|m| m.as_str().map(String::from)))
                .unwrap_or_else(|| body.chars().take(200).collect());
            let hint = match code {
                401 => " (is GSHEETS_TOKEN expired?)",
                403 => " (does the token have a spreadsheets scope and access to this file?)",
                404 => " (spreadsheet id not found or not shared with this account)",
                _ => "",
            };
            Error::from(format!("Google Sheets API returned {code}{hint}: {detail}"))
        }
        other => Error::from(format!("Google Sheets API request failed: {other}")),
    }
}
