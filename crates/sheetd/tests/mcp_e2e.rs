//! End-to-end MCP test: spawn the real `sheetd` binary, speak JSON-RPC over
//! its stdio, run a realistic task against a generated xlsx, and verify the
//! saved result by reopening it with the engine.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value as Json};
use sheetkit::book::{Book, Value};

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn spawn() -> McpClient {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sheetd"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sheetd");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        McpClient { child, stdin, stdout, next_id: 0 }
    }

    fn request(&mut self, method: &str, params: Json) -> Json {
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Json = serde_json::from_str(&line).expect("valid JSON response");
        assert_eq!(resp["id"], json!(self.next_id), "response id matches");
        resp
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Call a tool, asserting protocol success; returns (text, is_error).
    fn call_tool(&mut self, name: &str, args: Json) -> (String, bool) {
        let resp = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("tool call {name} returned protocol error: {resp}"));
        let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A 200-row orders workbook with one deliberate inconsistency.
fn build_fixture(path: &str) {
    let mut book = Book::new_empty("orders").unwrap();
    book.batch(|b| {
        b.set_input(0, 1, 1, "Order")?;
        b.set_input(0, 1, 2, "City")?;
        b.set_input(0, 1, 3, "Qty")?;
        b.set_input(0, 1, 4, "UnitPrice")?;
        let cities = ["Berlin", "Paris", "Madrid", "Rome"];
        for i in 0..200 {
            let row = i + 2;
            b.set_input(0, row, 1, &format!("ORD-{:04}", i + 1))?;
            b.set_input(0, row, 2, cities[(i as usize) % cities.len()])?;
            b.set_input(0, row, 3, &format!("{}", (i % 9) + 1))?;
            b.set_input(0, row, 4, &format!("{}.5", (i % 40) + 10))?;
        }
        // Row 150 has a bogus text value in Qty — the "messy export" artifact.
        b.set_input(0, 150, 3, "n/a")?;
        Ok(())
    })
    .unwrap();
    book.save(path, false).unwrap();
}

fn extract_workbook_id(open_text: &str) -> String {
    // "opened wb1 from …"
    let word = open_text
        .split_whitespace()
        .nth(1)
        .expect("workbook id in open response");
    word.to_string()
}

#[test]
fn full_task_over_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("orders.xlsx");
    let output = dir.path().join("orders-clean.xlsx");
    build_fixture(input.to_str().unwrap());

    let mut mcp = McpClient::spawn();

    // -- handshake ---------------------------------------------------------
    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "e2e-test", "version": "0" }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "sheetd");
    mcp.notify("notifications/initialized");

    let ping = mcp.request("ping", json!({}));
    assert!(ping["result"].is_object());

    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["sheet_open", "sheet_exec", "sheet_view", "sheet_save", "sheet_close"]
    );

    // -- open: the sketch must describe the data without any further reads --
    let (open_text, err) = mcp.call_tool("sheet_open", json!({ "path": input.to_str().unwrap() }));
    assert!(!err, "{open_text}");
    let id = extract_workbook_id(&open_text);
    assert!(open_text.contains("Order"), "sketch names headers: {open_text}");
    assert!(open_text.contains("200 rows + header"), "sketch counts rows: {open_text}");
    assert!(open_text.contains("mixed"), "sketch flags the dirty Qty column: {open_text}");

    // -- the task: clean, add a computed column, verify, sort ---------------
    let (fix_text, err) = mcp.call_tool(
        "sheet_exec",
        json!({
            "workbook_id": id,
            // Row 201 = order 200 (i=199): Qty (199%9)+1 = 2, UnitPrice 49.5.
            "script": "checkpoint before-cleanup\nfind \"n/a\" in C:C\nset C150 5\nset E1 Total\nset E2 =C2*D2\nfill E2 -> E2:E201\nexpect E2 == 10.5\nexpect E201 == 99",
        }),
    );
    assert!(!err, "{fix_text}");
    assert!(!fix_text.contains('✗'), "no line may fail: {fix_text}");
    assert!(fix_text.contains("C150"), "find located the artifact: {fix_text}");
    assert!(fix_text.contains("recalc:"), "delta echo present: {fix_text}");
    assert!(fix_text.contains("expect E2 == 10.5: OK"), "{fix_text}");
    assert!(fix_text.contains("expect E201 == 99: OK"), "{fix_text}");

    // -- a failing expectation stops the script and says so ----------------
    let (fail_text, err) = mcp.call_tool(
        "sheet_exec",
        json!({ "workbook_id": id, "script": "expect E2 == 999\nset A1 nope" }),
    );
    assert!(!err, "exec reports failures in-band: {fail_text}");
    assert!(fail_text.contains("expectation failed"), "{fail_text}");
    assert!(fail_text.contains("actual 10.5"), "{fail_text}");

    // -- aggregated view shows the fill as a formula fact -------------------
    let (view_text, err) = mcp.call_tool(
        "sheet_view",
        json!({ "workbook_id": id, "target": "table1", "mode": "aggregated" }),
    );
    assert!(!err, "{view_text}");
    assert!(view_text.contains("=C2*D2 fill E2:E201"), "{view_text}");

    // -- sort by a header name ----------------------------------------------
    let (sort_text, err) = mcp.call_tool(
        "sheet_exec",
        json!({ "workbook_id": id, "script": "sort table1 by Total desc\nexpect E2 >= 100" }),
    );
    assert!(!err, "{sort_text}");
    assert!(!sort_text.contains('✗'), "no line may fail: {sort_text}");
    assert!(sort_text.contains("sorted"), "{sort_text}");

    // -- save & close ---------------------------------------------------------
    let (save_text, err) = mcp.call_tool(
        "sheet_save",
        json!({ "workbook_id": id, "path": output.to_str().unwrap() }),
    );
    assert!(!err, "{save_text}");
    let (_, err) = mcp.call_tool("sheet_close", json!({ "workbook_id": id }));
    assert!(!err);

    // Closed workbooks reject further calls, in-band.
    let (gone_text, err) = mcp.call_tool(
        "sheet_exec",
        json!({ "workbook_id": id, "script": "sheets" }),
    );
    assert!(err, "closed workbook must error: {gone_text}");

    // -- verify the artifact independently -----------------------------------
    let book = Book::open(output.to_str().unwrap()).unwrap();
    assert_eq!(book.value(0, 1, 5), Value::Text("Total".into()));
    // Row 2 holds the largest Total after the desc sort, computed by formula.
    let top = match book.value(0, 2, 5) {
        Value::Number(n) => n,
        v => panic!("expected number, got {v:?}"),
    };
    assert!(top >= 100.0, "top total after sort: {top}");
    assert_eq!(
        book.formula(0, 2, 5).unwrap().as_deref(),
        Some("=C2*D2"),
        "sorted formulas stay row-anchored"
    );
    // The cleaned cell survived the round-trip as a number.
    let qty_col_has_na = (2..=201).any(|row| matches!(book.value(0, row, 3), Value::Text(_)));
    assert!(!qty_col_has_na, "no text left in Qty column");
}

#[test]
fn unknown_method_and_bad_tool() {
    let mut mcp = McpClient::spawn();
    mcp.request("initialize", json!({ "protocolVersion": "2025-06-18" }));
    mcp.notify("notifications/initialized");

    let resp = mcp.request("resources/list", json!({}));
    assert_eq!(resp["error"]["code"], -32601);

    let (text, err) = mcp.call_tool("sheet_nope", json!({}));
    assert!(err);
    assert!(text.contains("unknown tool"));

    let (text, err) = mcp.call_tool("sheet_exec", json!({ "workbook_id": "wb99", "script": "sheets" }));
    assert!(err);
    assert!(text.contains("no open workbook"), "{text}");
}
