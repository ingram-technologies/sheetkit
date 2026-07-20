//! End-to-end Google Sheets adapter test: a mock googleapis server (plain
//! TCP, three routes) + the real `sheetd mcp` binary pointed at it via
//! `GSHEETS_API_BASE`. Pull, edit, push — then inspect what would have hit
//! Google.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

// ---- a minimal HTTP mock of sheets.googleapis.com ---------------------------

#[derive(Default)]
struct Captured {
    batch_updates: Vec<Json>,
    auth_headers: Vec<String>,
}

fn spreadsheet_fixture() -> Json {
    json!({
        "spreadsheetId": "TESTID",
        "properties": { "title": "Remote Orders" },
        "sheets": [ {
            "properties": {
                "sheetId": 900,
                "title": "Orders",
                "gridProperties": { "rowCount": 50, "columnCount": 10 }
            },
            "data": [ {
                "rowData": [
                    { "values": [
                        { "userEnteredValue": { "stringValue": "Item" } },
                        { "userEnteredValue": { "stringValue": "Qty" } },
                        { "userEnteredValue": { "stringValue": "Price" } }
                    ]},
                    { "values": [
                        { "userEnteredValue": { "stringValue": "Ape" } },
                        { "userEnteredValue": { "numberValue": 2 } },
                        { "userEnteredValue": { "numberValue": 3.5 } }
                    ]},
                    { "values": [
                        { "userEnteredValue": { "stringValue": "Bee" } },
                        { "userEnteredValue": { "numberValue": 10 } },
                        { "userEnteredValue": { "numberValue": 0.4 } }
                    ]}
                ]
            } ]
        } ]
    })
}

fn props_fixture() -> Json {
    json!({ "sheets": [ {
        "properties": {
            "sheetId": 900,
            "title": "Orders",
            "gridProperties": { "rowCount": 50, "columnCount": 10 }
        }
    } ] })
}

/// Serve the three routes the adapter uses until the listener is dropped.
fn start_mock(captured: Arc<Mutex<Captured>>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            let mut content_length = 0usize;
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    break;
                }
                let lower = line.to_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    auth = line["authorization:".len()..].trim().to_string();
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).unwrap();
            }
            captured.lock().unwrap().auth_headers.push(auth);

            let response_body = if request_line.contains(":batchUpdate") {
                let parsed: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
                captured.lock().unwrap().batch_updates.push(parsed);
                json!({ "spreadsheetId": "TESTID", "replies": [] }).to_string()
            } else if request_line.contains("includeGridData=true") {
                spreadsheet_fixture().to_string()
            } else if request_line.contains("/v4/spreadsheets/TESTID") {
                props_fixture().to_string()
            } else {
                json!({ "error": { "message": "unknown route" } }).to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
        }
    });
    (base, handle)
}

// ---- MCP client over the spawned binary --------------------------------------

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn spawn(api_base: &str) -> McpClient {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sheetd"))
            .arg("mcp")
            .env("GSHEETS_API_BASE", api_base)
            .env("GSHEETS_TOKEN", "test-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sheetd");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        McpClient {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn call_tool(&mut self, name: &str, args: Json) -> (String, bool) {
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
            "params": { "name": name, "arguments": args } });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Json = serde_json::from_str(&line).expect("valid JSON");
        let result = &resp["result"];
        (
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            result["isError"].as_bool().unwrap_or(false),
        )
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn pull_edit_push_roundtrip() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let (base, _server) = start_mock(captured.clone());
    let mut mcp = McpClient::spawn(&base);

    // Init handshake (minimal).
    mcp.next_id += 1;
    let init = json!({ "jsonrpc": "2.0", "id": mcp.next_id, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" } });
    writeln!(mcp.stdin, "{init}").unwrap();
    mcp.stdin.flush().unwrap();
    let mut line = String::new();
    mcp.stdout.read_line(&mut line).unwrap();

    // -- pull ------------------------------------------------------------------
    let (open_text, err) = mcp.call_tool(
        "sheet_open",
        json!({ "path": "https://docs.google.com/spreadsheets/d/TESTID/edit#gid=0" }),
    );
    assert!(!err, "{open_text}");
    assert!(
        open_text.contains("from Google Sheets TESTID"),
        "{open_text}"
    );
    assert!(
        open_text.contains("Item"),
        "sketch has headers: {open_text}"
    );
    let id = open_text.split_whitespace().nth(1).unwrap().to_string();

    // -- edit locally: computed column + a clear ---------------------------------
    let (exec_text, err) = mcp.call_tool(
        "sheet_exec",
        json!({ "workbook_id": id, "script": "set D1 Total\nset D2:D3 =B2*C2\nclear A3\nexpect D2 == 7" }),
    );
    assert!(!err, "{exec_text}");
    assert!(exec_text.contains("expect D2 == 7: OK"), "{exec_text}");

    // -- push ----------------------------------------------------------------------
    let (save_text, err) = mcp.call_tool("sheet_save", json!({ "workbook_id": id }));
    assert!(!err, "{save_text}");
    assert!(save_text.contains("pushed 4 changed cells"), "{save_text}");
    assert!(save_text.contains("TESTID"), "{save_text}");

    {
        let cap = captured.lock().unwrap();
        assert_eq!(cap.batch_updates.len(), 1, "exactly one batchUpdate");
        assert!(
            cap.auth_headers.iter().all(|a| a == "Bearer test-token"),
            "{:?}",
            cap.auth_headers
        );
        let requests = cap.batch_updates[0]["requests"].as_array().unwrap().clone();
        // All updates target the remote sheetId from the pull.
        for r in &requests {
            assert_eq!(
                r.pointer("/updateCells/start/sheetId"),
                Some(&json!(900)),
                "{r}"
            );
        }
        // The formula column arrived as formulas.
        let formulas: Vec<&str> = requests
            .iter()
            .filter_map(|r| {
                r.pointer("/updateCells/rows/0/values/0/userEnteredValue/formulaValue")
                    .and_then(Json::as_str)
            })
            .collect();
        assert!(formulas.contains(&"=B2*C2"), "{requests:?}");
        assert!(formulas.contains(&"=B3*C3"), "{requests:?}");
        // The cleared cell went out as an empty CellData.
        let has_clear = requests.iter().any(|r| {
            r.pointer("/updateCells/start/rowIndex") == Some(&json!(2))
                && r.pointer("/updateCells/start/columnIndex") == Some(&json!(0))
                && r.pointer("/updateCells/rows/0/values/0") == Some(&json!({}))
        });
        assert!(has_clear, "{requests:?}");
    }

    // -- a second save with no edits pushes nothing --------------------------------
    let (save2, err) = mcp.call_tool("sheet_save", json!({ "workbook_id": id }));
    assert!(!err, "{save2}");
    assert!(save2.contains("no changes to push"), "{save2}");
    assert_eq!(captured.lock().unwrap().batch_updates.len(), 1);

    // -- incremental: edit again, only the new change goes out ----------------------
    let (_, err) = mcp.call_tool(
        "sheet_exec",
        json!({ "workbook_id": id, "script": "set B2 5" }),
    );
    assert!(!err);
    let (save3, err) = mcp.call_tool("sheet_save", json!({ "workbook_id": id }));
    assert!(!err, "{save3}");
    assert!(save3.contains("pushed 1 changed cell "), "{save3}");
    let cap = captured.lock().unwrap();
    let last = cap.batch_updates.last().unwrap()["requests"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(last.len(), 1, "{last:?}");
    assert_eq!(
        last[0].pointer("/updateCells/rows/0/values/0/userEnteredValue/numberValue"),
        Some(&json!(5.0)),
        "{last:?}"
    );
}
