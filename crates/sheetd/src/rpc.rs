//! Transport-independent MCP JSON-RPC dispatch, shared by the stdio loop and
//! the Streamable-HTTP endpoint.

use serde_json::{json, Value as Json};

use crate::tools::{tool_definitions, Tools};

pub const SERVER_NAME: &str = "sheetd";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Latest protocol revision this server knows; echoed back if the client
/// requests something unknown.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const KNOWN_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Handle one JSON-RPC message. Returns `None` for notifications (no reply).
/// `principal` labels the caller in exec output and highlights.
pub fn handle_message(tools: &mut Tools, msg: &Json, principal: &str) -> Option<Json> {
    let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(Json::Null);

    let response = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Json::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            let version = if KNOWN_PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                PROTOCOL_VERSION
            };
            result_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
                }),
            )
        }
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params.get("name").and_then(Json::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools.call(name, &args, principal) {
                Ok(text) => result_response(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                ),
                // Tool-level failures are results with isError, not protocol
                // errors — the model is meant to read them.
                Err(e) => result_response(
                    id,
                    json!({ "content": [{ "type": "text", "text": e.0 }], "isError": true }),
                ),
            }
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

pub fn result_response(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
