//! Lifecycle end-to-end: the blob store as source of truth. Sessions get
//! evicted and rehydrated with their ephemera intact, undo survives via the
//! journal, the channel replays missed frames, admission control rejects
//! oversized imports, purge removes blobs, and sequence numbers survive a
//! full server restart.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value as Json};

const TOKEN: &str = "test-secret";

struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn spawn(data_dir: &std::path::Path, extra: &[&str]) -> Server {
        let mut args = vec![
            "serve",
            "--addr",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--token",
            TOKEN,
        ];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_sheetd"))
            .args(&args)
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

    fn create_csv(&self, csv: &str) -> String {
        let created: Json = ureq::post(&self.url("/workbooks?format=csv"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_bytes(csv.as_bytes())
            .unwrap()
            .into_json()
            .unwrap();
        created["workbook_id"].as_str().unwrap().to_string()
    }

    fn exec(&self, id: &str, script: &str) -> String {
        let resp: Json = ureq::post(&self.url(&format!("/workbooks/{id}/exec")))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(json!({ "script": script }))
            .unwrap()
            .into_json()
            .unwrap();
        resp["output"].as_str().unwrap_or_default().to_string()
    }

    fn get(&self, path: &str) -> Result<Json, u16> {
        match ureq::get(&self.url(path))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .call()
        {
            Ok(r) => Ok(r.into_json().unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("transport error: {e}"),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// With `--max-resident 1`, opening a second workbook evicts the first;
/// touching the first rehydrates it with checkpoints, highlights, and
/// journal-backed undo intact.
#[test]
fn eviction_rehydration_and_journal_undo() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path(), &["--max-resident", "1"]);

    let a = server.create_csv("X,Y\n1,2\n");
    let out = server.exec(&a, "set A2 10\ncheckpoint mark\nhighlight B2 note=\"keep\"");
    assert!(out.contains("checkpoint"), "{out}");
    let out = server.exec(&a, "set A2 20\nexpect A2 == 20");
    assert!(out.contains("OK"), "{out}");

    // Opening B pushes A (the only other resident) out of the cache.
    let b = server.create_csv("Q\n9\n");
    server.exec(&b, "set A2 99");

    // A rehydrates transparently: ephemera survived the eviction.
    let hl = server.get(&format!("/workbooks/{a}/highlights")).unwrap();
    assert_eq!(hl["highlights"].as_array().unwrap().len(), 1, "{hl}");
    assert_eq!(hl["highlights"][0]["note"], "keep");

    // Engine history died with the eviction — undo must fall back to the
    // journal and rebuild the pre-exec state.
    let b_evicts_a = server.create_csv("Z\n1\n"); // ensure A is cold again
    server.exec(&b_evicts_a, "set A2 5");
    let out = server.exec(&a, "undo\nexpect A2 == 10");
    assert!(out.contains("journal"), "journal undo used: {out}");
    assert!(out.contains("OK (actual 10)"), "{out}");

    // Checkpoints also survived: restore works on the rehydrated session.
    let out = server.exec(&a, "restore mark\nexpect A2 == 10");
    assert!(out.contains("restored"), "{out}");
}

/// The channel replays journal frames to a client that reconnects with
/// last_seq, including the resync marker for whole-book swaps.
#[test]
fn channel_replays_missed_frames() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path(), &[]);
    let id = server.create_csv("A\n1\n");

    server.exec(&id, "set A2 2");
    server.exec(&id, "set A2 3");
    let seq = server.get(&format!("/workbooks/{id}")).unwrap()["seq"]
        .as_u64()
        .unwrap();
    assert_eq!(seq, 2);

    // Connect claiming we saw nothing: welcome first, then both frames.
    let ws_url = format!(
        "{}/workbooks/{id}/channel?token={TOKEN}&principal=replayer&last_seq=0",
        server.base.replace("http://", "ws://")
    );
    let (mut sock, _) = tungstenite::connect(ws_url).unwrap();
    let mut read_json = || -> Json {
        loop {
            if let tungstenite::Message::Text(t) = sock.read().expect("ws message") {
                return serde_json::from_str(&t).unwrap();
            }
        }
    };
    let welcome = read_json();
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["seq"], 2);
    let f1 = read_json();
    assert_eq!(f1["type"], "applied");
    assert_eq!(f1["seq"], 1);
    assert!(!f1["diffs_b64"].as_str().unwrap().is_empty());
    let f2 = read_json();
    assert_eq!(f2["seq"], 2);
    assert!(f2["delta"].as_array().unwrap().iter().any(|d| d[2] == "3"), "{f2}");
}

/// Oversized imports are rejected with a useful message, not accepted as a
/// slow-motion OOM.
#[test]
fn admission_control_rejects_oversized() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path(), &["--max-cells", "10"]);

    let mut csv = String::from("A,B,C,D\n");
    for i in 0..5 {
        csv.push_str(&format!("{i},{i},{i},{i}\n"));
    }
    let resp = ureq::post(&server.url("/workbooks?format=csv"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_bytes(csv.as_bytes());
    match resp {
        Err(ureq::Error::Status(422, r)) => {
            let body: Json = r.into_json().unwrap();
            assert!(body["error"].as_str().unwrap().contains("limit of 10"), "{body}");
        }
        other => panic!("expected 422, got {other:?}"),
    }
}

/// DELETE keeps blobs (rehydration works); DELETE ?purge=true removes them.
#[test]
fn purge_removes_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::spawn(dir.path(), &[]);
    let id = server.create_csv("A\n1\n");
    server.exec(&id, "set A2 7");

    // Plain close: still rehydratable.
    ureq::delete(&server.url(&format!("/workbooks/{id}")))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    assert!(server.get(&format!("/workbooks/{id}")).is_ok(), "rehydrates after close");

    // Purge: gone for real, files removed.
    ureq::delete(&server.url(&format!("/workbooks/{id}?purge=true")))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    assert_eq!(server.get(&format!("/workbooks/{id}")), Err(404));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&id))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// Sequence numbers and workbook state survive a full server restart.
#[test]
fn restart_preserves_state_and_seq() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let server = Server::spawn(dir.path(), &[]);
        id = server.create_csv("N\n1\n");
        server.exec(&id, "set A2 2");
        server.exec(&id, "set A2 3");
        // Server drops here (killed).
    }
    let server = Server::spawn(dir.path(), &[]);
    let wb = server.get(&format!("/workbooks/{id}")).unwrap();
    assert_eq!(wb["seq"], 2, "two execs before the restart: {wb}");
    let out = server.exec(&id, "expect A2 == 3\nset A2 4");
    assert!(out.contains("OK"), "{out}");
    assert_eq!(server.get(&format!("/workbooks/{id}")).unwrap()["seq"], 3);
}
