//! MCP server over stdio: newline-delimited JSON-RPC 2.0.
//!
//! Implements the core of the Model Context Protocol — initialize handshake,
//! tools/list, tools/call, ping — which is everything a tools-only server
//! needs. Diagnostics go to stderr; stdout carries protocol frames only.

use std::io::{BufRead, Write};

use serde_json::{json, Value as Json};

use crate::tools::{tool_definitions, Tools};

const SERVER_NAME: &str = "sheetd";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Latest protocol revision this server knows; echoed back if the client
/// requests something unknown.
const PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut tools = Tools::new();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Json = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                let resp = error_response(Json::Null, -32700, &format!("parse error: {e}"));
                write_msg(&stdout, &resp)?;
                continue;
            }
        };
        let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Json::Null);

        // Notifications (no id) get no response.
        let Some(id) = id else {
            if method == "notifications/initialized" || method.starts_with("notifications/") {
                continue;
            }
            continue;
        };

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
                match tools.call(name, &args) {
                    Ok(text) => result_response(
                        id,
                        json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                    ),
                    // Tool-level failures are results with isError, not
                    // protocol errors — the model is meant to read them.
                    Err(e) => result_response(
                        id,
                        json!({ "content": [{ "type": "text", "text": e.0 }], "isError": true }),
                    ),
                }
            }
            other => error_response(id, -32601, &format!("method not found: {other}")),
        };
        write_msg(&stdout, &response)?;
    }
    Ok(())
}

fn result_response(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_msg(stdout: &std::io::Stdout, msg: &Json) -> std::io::Result<()> {
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, msg)?;
    handle.write_all(b"\n")?;
    handle.flush()
}
