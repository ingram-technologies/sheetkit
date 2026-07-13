//! MCP server over stdio: newline-delimited JSON-RPC 2.0. Dispatch lives in
//! [`crate::rpc`]; this is just the transport loop. Diagnostics go to stderr;
//! stdout carries protocol frames only.

use std::io::{BufRead, Write};

use serde_json::Value as Json;

use crate::rpc::{error_response, handle_message};
use crate::tools::Tools;

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
        if let Some(response) = handle_message(&mut tools, &msg, "agent") {
            write_msg(&stdout, &response)?;
        }
    }
    Ok(())
}

fn write_msg(stdout: &std::io::Stdout, msg: &Json) -> std::io::Result<()> {
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, msg)?;
    handle.write_all(b"\n")?;
    handle.flush()
}
