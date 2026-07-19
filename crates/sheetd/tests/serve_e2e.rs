//! End-to-end test for `sheetd serve`: spawn the real binary and exercise all
//! three doors — REST workbook API, MCP over streamable HTTP, and the
//! WebSocket channel — against one shared workbook session.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value as Json};

const TOKEN: &str = "test-secret";

struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn spawn(data_dir: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sheetd"))
            .args([
                "serve",
                "--addr",
                "127.0.0.1:0",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--token",
                TOKEN,
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sheetd serve");
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let base = line
            .trim()
            .strip_prefix("sheetd listening on ")
            .unwrap_or_else(|| panic!("unexpected banner: {line}"))
            .to_string();
        Server { child, base }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.replace("http://", "ws://"))
    }

    fn post_json(&self, path: &str, body: Json) -> Json {
        ureq::post(&self.url(path))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .header("x-principal", "resty")
            .send_json(body)
            .expect("POST ok")
            .into_body()
            .read_json()
            .expect("JSON response")
    }

    fn get_json(&self, path: &str) -> Json {
        ureq::get(&self.url(path))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .call()
            .expect("GET ok")
            .into_body()
            .read_json()
            .expect("JSON response")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn three_doors_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path());

    // -- health is open, everything else needs the token --------------------
    let health: Json = ureq::get(&server.url("/health"))
        .call()
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    assert_eq!(health["ok"], json!(true));
    assert_eq!(health["channel_protocol"], "sheets.channel.v1");

    // A ureq agent that returns non-2xx as `Ok` (body intact) instead of a
    // bodyless `Err(StatusCode)`, so we can assert on the status ourselves.
    let no_status_err = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let denied = no_status_err.get(&server.url("/workbooks")).call();
    match denied {
        Ok(r) if r.status().as_u16() == 401 => {}
        other => panic!("expected 401, got {other:?}"),
    }

    // -- REST door: create from CSV ------------------------------------------
    let csv = "Item,Qty,Price\nApe,2,3.5\nBee,10,0.4\nCat,1,12\n";
    let created: Json = ureq::post(&server.url("/workbooks?name=orders&format=csv"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send(csv.as_bytes())
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    let id = created["workbook_id"].as_str().unwrap().to_string();
    assert!(id.starts_with("wb-"), "{id}");
    assert!(created["sketch"].as_str().unwrap().contains("3 rows + header"));

    // -- MCP door works against the same workbook ----------------------------
    let mcp = |body: Json| -> Json {
        ureq::post(&server.url("/mcp"))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .header("x-principal", "mcp-agent")
            .send_json(body)
            .unwrap()
            .into_body()
            .read_json()
            .unwrap()
    };
    let init = mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" } }));
    assert_eq!(init["result"]["serverInfo"]["name"], "sheetd");

    let tools_list = mcp(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    assert_eq!(tools_list["result"]["tools"].as_array().unwrap().len(), 5);

    let exec = mcp(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "sheet_exec",
        "arguments": { "workbook_id": id, "script": "set D1 Total\nset D2:D4 =B2*C2\nexpect D4 == 12" }
    }}));
    let text = exec["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(exec["result"]["isError"], json!(false));
    assert!(text.contains("expect D4 == 12: OK"), "{text}");

    // Notifications return 202 with no body.
    let resp = ureq::post(&server.url("/mcp"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .unwrap();
    assert_eq!(resp.status(), 202);

    // -- REST view + exec on the MCP-created column ---------------------------
    let view = server.get_json(&format!("/workbooks/{id}/view?target=A1:D4"));
    assert!(view["view"].as_str().unwrap().contains("=B2*C2 ⇒ 7"), "{view}");

    let exec2 = server.post_json(
        &format!("/workbooks/{id}/exec"),
        json!({ "script": "set B2 4\nexpect D2 == 14" }),
    );
    assert!(exec2["output"].as_str().unwrap().contains("OK"), "{exec2}");

    // -- file export round-trips ------------------------------------------------
    let mut xlsx: Vec<u8> = Vec::new();
    ureq::get(&server.url(&format!("/workbooks/{id}/file?format=xlsx")))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .into_body()
        .into_reader()
        .read_to_end(&mut xlsx)
        .unwrap();
    assert!(xlsx.starts_with(b"PK"), "xlsx magic");
    let book = sheetkit::book::Book::from_xlsx_bytes(&xlsx, "check").unwrap();
    assert_eq!(book.value(0, 2, 4), sheetkit::book::Value::Number(14.0));

    // -- persistence: close, then rehydrate from the blob store ----------------
    let closed = ureq::delete(&server.url(&format!("/workbooks/{id}")))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .into_body()
        .read_json::<Json>()
        .unwrap();
    assert_eq!(closed["closed"].as_str().unwrap(), id);
    let back = server.get_json(&format!("/workbooks/{id}"));
    assert!(back["sketch"].as_str().unwrap().contains("Total"), "rehydrated: {back}");
}

#[test]
fn channel_streams_applied_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path());

    let created: Json = ureq::post(&server.url("/workbooks?name=live&format=csv"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send(b"A,B\n1,2\n" as &[u8])
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    let id = created["workbook_id"].as_str().unwrap().to_string();

    // Connect two clients: one acts, one watches.
    let (mut actor, _) = tungstenite::connect(server.ws_url(&format!(
        "/workbooks/{id}/channel?token={TOKEN}&principal=actor"
    )))
    .expect("actor connects");
    let (mut watcher, _) = tungstenite::connect(server.ws_url(&format!(
        "/workbooks/{id}/channel?token={TOKEN}&principal=watcher"
    )))
    .expect("watcher connects");

    let read_json = |sock: &mut tungstenite::WebSocket<_>| -> Json {
        loop {
            match sock.read().expect("ws message") {
                tungstenite::Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                _ => continue,
            }
        }
    };

    let welcome = read_json(&mut actor);
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["v"], "sheets.channel.v1");
    // The exact engine version moves with the pin; replicas only need it
    // present and stable, so assert shape rather than value.
    assert!(
        welcome["engine_version"].as_str().is_some_and(|v| !v.is_empty()),
        "{welcome}"
    );
    let welcome2 = read_json(&mut watcher);
    assert_eq!(welcome2["type"], "welcome");

    // The actor runs a command; both clients see the applied fan-out.
    actor
        .send(tungstenite::Message::Text(
            json!({ "type": "cmd", "id": "c1", "script": "set C1 Sum\nset C2 =A2+B2" })
                .to_string(),
        ))
        .unwrap();

    let mut saw = (false, false, false); // executing, applied, idle
    for _ in 0..12 {
        let msg = read_json(&mut watcher);
        match msg["type"].as_str().unwrap_or("") {
            "agent.status" if msg["phase"] == "executing" => saw.0 = true,
            "applied" => {
                assert_eq!(msg["principal"], "actor");
                assert_eq!(msg["cmd_id"], "c1");
                assert!(msg["seq"].as_u64().unwrap() >= 1);
                let delta = msg["delta"].as_array().unwrap();
                assert!(
                    delta.iter().any(|d| d[0].as_str().unwrap().ends_with("C2") && d[2] == "3"),
                    "{msg}"
                );
                assert!(!msg["diffs_b64"].as_str().unwrap().is_empty(), "diff blob present");
                saw.1 = true;
            }
            "agent.status" if msg["phase"] == "idle" => {
                saw.2 = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(saw, (true, true, true), "full exec lifecycle seen");

    // Highlights fan out and land in the session.
    actor
        .send(tungstenite::Message::Text(
            json!({ "type": "highlight.set", "range": "C2", "color": "green", "note": "check me" })
                .to_string(),
        ))
        .unwrap();
    let mut got_highlight = false;
    for _ in 0..8 {
        let msg = read_json(&mut watcher);
        if msg["type"] == "highlight.set" {
            assert_eq!(msg["author"], "actor");
            assert!(msg["range"].as_str().unwrap().ends_with("C2"));
            got_highlight = true;
            break;
        }
    }
    assert!(got_highlight);
    let listed = server.get_json(&format!("/workbooks/{id}/highlights"));
    assert_eq!(listed["highlights"].as_array().unwrap().len(), 1);

    // A failing script produces a rejected frame, not silence.
    actor
        .send(tungstenite::Message::Text(
            json!({ "type": "cmd", "id": "c2", "script": "expect C2 == 999" })
                .to_string(),
        ))
        .unwrap();
    let mut got_rejected = false;
    for _ in 0..8 {
        let msg = read_json(&mut watcher);
        if msg["type"] == "rejected" {
            assert_eq!(msg["cmd_id"], "c2");
            assert!(msg["error"].as_str().unwrap().contains("actual 3"), "{msg}");
            got_rejected = true;
            break;
        }
    }
    assert!(got_rejected);
}
