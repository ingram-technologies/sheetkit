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

/// A ureq agent that surfaces non-2xx responses as `Ok` (with the body still
/// attached) instead of a bodyless `Err(StatusCode)`, so `describe_http_error`
/// can extract Google's `/error/message`. Transport failures still return `Err`.
fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    )
}

fn get(url: &str, token: &str) -> Result<Json> {
    let mut response = agent()
        .get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(describe_transport_error)?;
    if !response.status().is_success() {
        return Err(describe_http_error(response));
    }
    response
        .body_mut()
        .read_json()
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
    let mut response = agent()
        .post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "requests": requests }))
        .map_err(describe_transport_error)?;
    if !response.status().is_success() {
        return Err(describe_http_error(response));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| Error::from(format!("bad JSON from Google Sheets API: {e}")))
}

/// A transport (network) failure — DNS, connection, TLS. No HTTP status.
fn describe_transport_error(e: ureq::Error) -> Error {
    Error::from(format!("Google Sheets API request failed: {e}"))
}

/// A non-2xx HTTP response whose body we still hold (thanks to the
/// `http_status_as_error(false)` agent). Extracts Google's `/error/message`,
/// falling back to the first 200 chars of the body.
fn describe_http_error(response: ureq::http::Response<ureq::Body>) -> Error {
    let code = response.status().as_u16();
    let body = response.into_body().read_to_string().unwrap_or_default();
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
