//! `sheetd serve` — one engine service, three network doors:
//!
//! - **`POST /mcp`** — MCP over streamable HTTP (stateless JSON responses),
//!   the same five tools as stdio.
//! - **REST workbook API** — `POST /workbooks`, `GET /workbooks/{id}`,
//!   `POST /workbooks/{id}/exec`, `GET /workbooks/{id}/view`,
//!   `GET|PUT /workbooks/{id}/file`, `GET /workbooks/{id}/highlights`,
//!   `DELETE /workbooks/{id}`.
//! - **`GET /workbooks/{id}/channel`** — WebSocket realtime channel
//!   (`sheets.channel.v1`): every applied script fans out with its recalc
//!   delta and the engine's diff blob, so same-version replicas can mirror
//!   the workbook live; presence and highlights ride the same channel.
//!
//! State: one authoritative session per workbook, all commands serialized
//! through it; blobs persist to `--data-dir` and rehydrate on demand. Auth is
//! a static bearer token (`--token` / `SHEETD_TOKEN`); the principal label is
//! read from a configurable header (`SHEETD_PRINCIPAL_HEADER`, default
//! `x-principal`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json as AxumJson, Router};
use serde_json::{json, Value as Json};
use tokio::sync::broadcast;

use crate::rpc;
use crate::tools::{ExecEvent, ExecObserver, Tools};

pub const CHANNEL_PROTOCOL: &str = "sheets.channel.v1";
/// The engine build this binary embeds. Diff blobs, `.ic` snapshots, and the
/// journal are version-locked: replicas (including any wasm build) must be
/// compiled from the same engine source (see UPSTREAM.md). Since we pin a git
/// revision rather than a release, the version string carries the short rev.
pub const ENGINE_VERSION: &str = "0.7.1-git.9ee7e066";

pub struct ServeOptions {
    pub addr: String,
    pub data_dir: Option<std::path::PathBuf>,
    pub token: Option<String>,
    /// Evict least-recently-used sessions past this count (0 = unlimited).
    pub max_resident: usize,
    /// Drop sessions idle longer than this many seconds (0 = never).
    pub idle_secs: u64,
    /// Garbage-collect blobs untouched for this many days (0 = keep forever).
    pub gc_days: u64,
    /// Reject imports with more non-empty cells than this (0 = unlimited).
    pub max_cells: u64,
}

// ---- realtime channels ------------------------------------------------------

#[derive(Default)]
struct Channels {
    inner: StdMutex<HashMap<String, broadcast::Sender<String>>>,
}

impl Channels {
    fn entry(&self, id: &str) -> broadcast::Sender<String> {
        let mut map = self.inner.lock().unwrap();
        map.entry(id.to_string())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    /// Broadcast a message. Sequenced frames carry their `seq` already
    /// (assigned durably by the tools layer).
    fn publish(&self, id: &str, msg: Json) {
        let _ = self.entry(id).send(msg.to_string());
    }
}

/// Fans exec lifecycle out to the workbook's channel.
struct Broadcaster(Arc<Channels>);

impl ExecObserver for Broadcaster {
    fn exec_started(&self, workbook_id: &str, principal: &str, script: &str) {
        let first_line = script.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        self.0.publish(
            workbook_id,
            json!({
                "type": "agent.status",
                "principal": principal,
                "phase": "executing",
                "script_line": first_line,
            }),
        );
    }

    fn exec_finished(&self, e: &ExecEvent) {
        // seq > 0 means the exec changed state and was journaled; the frame
        // broadcast here is byte-identical to the journal line (replayable).
        if e.seq > 0 {
            self.0.publish(&e.workbook_id, crate::tools::applied_frame(e));
        }
        if let Some((line, error)) = &e.error {
            self.0.publish(
                &e.workbook_id,
                json!({
                    "type": "rejected",
                    "principal": e.principal,
                    "cmd_id": e.cmd_id,
                    "line": line,
                    "error": error,
                }),
            );
        }
        self.0.publish(
            &e.workbook_id,
            json!({ "type": "agent.status", "principal": e.principal, "phase": "idle" }),
        );
    }
}

// ---- app state ---------------------------------------------------------------

pub struct AppState {
    tools: tokio::sync::Mutex<Tools>,
    channels: Arc<Channels>,
    token: Option<String>,
    principal_header: String,
}

impl AppState {
    fn authorized(&self, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        bearer == Some(expected.as_str()) || query.get("token").map(String::as_str) == Some(expected)
    }

    fn principal(&self, headers: &HeaderMap, query: &HashMap<String, String>) -> String {
        headers
            .get(&self.principal_header)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .or_else(|| query.get("principal").cloned())
            .unwrap_or_else(|| "agent".to_string())
    }
}

pub fn run(opts: ServeOptions) -> std::io::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_run(opts))
}

async fn async_run(opts: ServeOptions) -> std::io::Result<()> {
    if let Some(dir) = &opts.data_dir {
        std::fs::create_dir_all(dir)?;
    }
    let channels = Arc::new(Channels::default());
    let mut tools = match &opts.data_dir {
        Some(dir) => Tools::with_store(dir.clone()),
        None => Tools::new(),
    };
    tools.random_ids = true;
    tools.max_cells = opts.max_cells;
    tools.max_resident = opts.max_resident;
    tools.observer = Some(Box::new(Broadcaster(channels.clone())));

    let state = Arc::new(AppState {
        tools: tokio::sync::Mutex::new(tools),
        channels,
        token: opts.token,
        principal_header: std::env::var("SHEETD_PRINCIPAL_HEADER")
            .unwrap_or_else(|_| "x-principal".to_string())
            .to_lowercase(),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/mcp", get(mcp_get).post(mcp_post))
        .route("/workbooks", get(list_workbooks).post(create_workbook))
        .route("/workbooks/{id}", get(get_workbook).delete(delete_workbook))
        .route("/workbooks/{id}/exec", axum::routing::post(exec_workbook))
        .route("/workbooks/{id}/view", get(view_workbook))
        .route("/workbooks/{id}/file", get(get_file).put(put_file))
        .route("/workbooks/{id}/highlights", get(get_highlights))
        .route("/workbooks/{id}/channel", get(channel_ws))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state.clone());

    // Housekeeping: idle-session eviction and blob GC.
    if opts.idle_secs > 0 || opts.gc_days > 0 {
        let sweeper_state = state.clone();
        let idle = std::time::Duration::from_secs(opts.idle_secs);
        let gc_age = std::time::Duration::from_secs(opts.gc_days * 24 * 3600);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let mut tools = sweeper_state.tools.lock().await;
                if !idle.is_zero() {
                    let evicted = tools.evict_idle(idle);
                    if !evicted.is_empty() {
                        eprintln!("sheetd: evicted idle sessions: {}", evicted.join(", "));
                    }
                }
                if !gc_age.is_zero() {
                    if let Some(store) = &tools.store {
                        let resident = tools.manager.ids();
                        let removed = store.gc(gc_age, &resident);
                        if !removed.is_empty() {
                            eprintln!("sheetd: gc removed: {}", removed.join(", "));
                        }
                    }
                }
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&opts.addr).await?;
    // Parsed by clients and tests; keep the format stable.
    println!("sheetd listening on http://{}", listener.local_addr()?);
    use std::io::Write;
    std::io::stdout().flush()?;
    axum::serve(listener, app).await
}

// ---- basic handlers ---------------------------------------------------------

async fn health() -> impl IntoResponse {
    AxumJson(json!({
        "ok": true,
        "server": rpc::SERVER_NAME,
        "version": rpc::SERVER_VERSION,
        "engine": ENGINE_VERSION,
        "channel_protocol": CHANNEL_PROTOCOL,
    }))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, AxumJson(json!({ "error": "missing or bad bearer token" }))).into_response()
}

fn err_response(status: StatusCode, msg: &str) -> Response {
    (status, AxumJson(json!({ "error": msg }))).into_response()
}

// ---- MCP over streamable HTTP -------------------------------------------------

async fn mcp_get() -> Response {
    // We don't push server-initiated messages; the spec allows refusing GET.
    err_response(StatusCode::METHOD_NOT_ALLOWED, "this server does not offer an SSE stream; POST JSON-RPC messages to /mcp")
}

async fn mcp_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let msg: Json = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                AxumJson(rpc::error_response(Json::Null, -32700, &format!("parse error: {e}"))),
            )
                .into_response()
        }
    };
    if msg.is_array() {
        return (
            StatusCode::BAD_REQUEST,
            AxumJson(rpc::error_response(Json::Null, -32600, "batching is not supported")),
        )
            .into_response();
    }
    let principal = state.principal(&headers, &query);
    let mut tools = state.tools.lock().await;
    match rpc::handle_message(&mut tools, &msg, &principal) {
        Some(response) => AxumJson(response).into_response(),
        // Notification: no body, per streamable HTTP.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

// ---- REST workbook API ---------------------------------------------------------

async fn list_workbooks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let tools = state.tools.lock().await;
    AxumJson(json!({ "workbooks": tools.manager.ids() })).into_response()
}

async fn create_workbook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let name = query.get("name").cloned().unwrap_or_else(|| "workbook".to_string());
    let format = query.get("format").cloned().unwrap_or_else(|| {
        if body.starts_with(b"PK") {
            "xlsx".to_string()
        } else {
            "csv".to_string()
        }
    });
    let mut tools = state.tools.lock().await;
    match tools.open_bytes(&name, &format, &body) {
        Ok(id) => {
            let sketch = tools
                .session(&id)
                .map(|s| {
                    let (regions, _) = s.regions().clone();
                    sheetkit::view::sketch(&s.book, &regions)
                })
                .unwrap_or_default();
            (StatusCode::CREATED, AxumJson(json!({ "workbook_id": id, "sketch": sketch }))).into_response()
        }
        Err(e) => err_response(StatusCode::UNPROCESSABLE_ENTITY, &e.0),
    }
}

async fn get_workbook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let mut tools = state.tools.lock().await;
    let seq = tools.current_seq(&id);
    match tools.session(&id) {
        Ok(session) => {
            let (regions, _) = session.regions().clone();
            let sketch = sheetkit::view::sketch(&session.book, &regions);
            AxumJson(json!({
                "workbook_id": id,
                "sheets": session.book.sheet_names(),
                "sketch": sketch,
                "seq": seq,
            }))
            .into_response()
        }
        Err(e) => err_response(StatusCode::NOT_FOUND, &e.0),
    }
}

async fn delete_workbook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let mut tools = state.tools.lock().await;
    let purge = query.get("purge").map(String::as_str) == Some("true");
    let result = if purge { tools.purge(&id) } else { tools.close(&id) };
    match result {
        Ok(_) => AxumJson(json!({ "closed": id, "purged": purge })).into_response(),
        Err(e) => err_response(StatusCode::NOT_FOUND, &e.0),
    }
}

async fn exec_workbook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let payload: Json = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, &format!("bad JSON body: {e}")),
    };
    let Some(script) = payload.get("script").and_then(Json::as_str) else {
        return err_response(StatusCode::BAD_REQUEST, "body must be {\"script\": \"…\"}");
    };
    let cmd_id = payload.get("cmd_id").and_then(Json::as_str);
    let principal = state.principal(&headers, &query);
    let mut tools = state.tools.lock().await;
    match tools.exec(&id, script, &principal, cmd_id) {
        Ok(output) => AxumJson(json!({ "workbook_id": id, "output": output })).into_response(),
        Err(e) => err_response(StatusCode::NOT_FOUND, &e.0),
    }
}

async fn view_workbook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let Some(target) = query.get("target") else {
        return err_response(StatusCode::BAD_REQUEST, "pass ?target=<range|region|sheet>");
    };
    let budget = query.get("budget").and_then(|b| b.parse().ok());
    let mut tools = state.tools.lock().await;
    match tools.view(&id, target, query.get("mode").map(String::as_str), budget) {
        Ok(view) => AxumJson(json!({ "workbook_id": id, "view": view })).into_response(),
        Err(e) => err_response(StatusCode::UNPROCESSABLE_ENTITY, &e.0),
    }
}

async fn get_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let format = query.get("format").cloned().unwrap_or_else(|| "xlsx".to_string());
    let mut tools = state.tools.lock().await;
    match tools.export_bytes(&id, &format) {
        Ok(bytes) => {
            let content_type = match format.as_str() {
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "csv" => "text/csv; charset=utf-8",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(e) => err_response(StatusCode::NOT_FOUND, &e.0),
    }
}

async fn put_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let format = query.get("format").cloned().unwrap_or_else(|| {
        if body.starts_with(b"PK") {
            "xlsx".to_string()
        } else {
            "csv".to_string()
        }
    });
    let principal = state.principal(&headers, &query);
    let mut tools = state.tools.lock().await;
    match tools.replace_bytes(&id, &format, &body, &principal) {
        Ok(()) => AxumJson(json!({ "workbook_id": id, "replaced": true })).into_response(),
        Err(e) => err_response(StatusCode::UNPROCESSABLE_ENTITY, &e.0),
    }
}

fn highlight_json(h: &sheetkit::session::Highlight) -> Json {
    json!({
        "id": h.id,
        "range": format!("{}!{}", sheetkit::addr::display_sheet(&h.sheet_name), h.range.a1()),
        "color": h.color,
        "note": h.note,
        "author": h.author,
        "resolved": h.resolved,
    })
}

async fn get_highlights(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let mut tools = state.tools.lock().await;
    match tools.session(&id) {
        Ok(session) => {
            let list: Vec<Json> = session.highlights.iter().map(highlight_json).collect();
            AxumJson(json!({ "workbook_id": id, "highlights": list })).into_response()
        }
        Err(e) => err_response(StatusCode::NOT_FOUND, &e.0),
    }
}

// ---- the realtime channel -------------------------------------------------------

async fn channel_ws(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.authorized(&headers, &query) {
        return unauthorized();
    }
    let principal = {
        // Browsers cannot set headers on WebSocket; prefer ?principal=.
        query
            .get("principal")
            .cloned()
            .unwrap_or_else(|| state.principal(&headers, &query))
    };
    let last_seq: Option<u64> = query.get("last_seq").and_then(|v| v.parse().ok());
    ws.on_upgrade(move |socket| channel_loop(socket, state, id, principal, last_seq))
}

async fn welcome_message(state: &Arc<AppState>, id: &str) -> Result<Json, String> {
    let mut tools = state.tools.lock().await;
    let seq = tools.current_seq(id);
    let session = tools.session(id).map_err(|e| e.0)?;
    let highlights: Vec<Json> = session.highlights.iter().map(highlight_json).collect();
    Ok(json!({
        "type": "welcome",
        "v": CHANNEL_PROTOCOL,
        "workbook_id": id,
        "engine_version": ENGINE_VERSION,
        "seq": seq,
        "sheets": session.book.sheet_names(),
        "highlights": highlights,
    }))
}

async fn channel_loop(
    mut socket: WebSocket,
    state: Arc<AppState>,
    id: String,
    principal: String,
    last_seq: Option<u64>,
) {
    // The workbook must exist (or rehydrate) before we subscribe.
    let welcome = match welcome_message(&state, &id).await {
        Ok(w) => w,
        Err(e) => {
            let _ = socket
                .send(Message::Text(json!({ "type": "error", "error": e }).to_string().into()))
                .await;
            return;
        }
    };
    let tx = state.channels.entry(&id);
    let mut rx = tx.subscribe();
    if socket.send(Message::Text(welcome.to_string().into())).await.is_err() {
        return;
    }
    // Reconnect replay: stream the journal frames the client missed. A
    // `resync: true` frame in the replay tells it to refetch the file instead
    // of applying diffs.
    if let Some(after) = last_seq {
        let frames = {
            let tools = state.tools.lock().await;
            tools.frames_after(&id, after)
        };
        for frame in frames {
            if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                return;
            }
        }
    }
    state.channels.publish(
        &id,
        json!({ "type": "presence", "principal": principal, "joined": true }),
    );

    loop {
        tokio::select! {
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The client missed messages; tell it to resync.
                        let msg = json!({ "type": "gap", "missed": n, "hint": "refetch /workbooks/{id}/file and resubscribe" });
                        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(parsed) = serde_json::from_str::<Json>(&text) else {
                    let _ = socket.send(Message::Text(json!({ "type": "error", "error": "messages must be JSON" }).to_string().into())).await;
                    continue;
                };
                if let Some(reply) = handle_channel_message(&state, &id, &principal, &parsed).await {
                    if socket.send(Message::Text(reply.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    state.channels.publish(
        &id,
        json!({ "type": "presence", "principal": principal, "left": true }),
    );
}

/// Handle one client→server channel message. Returns a direct reply for the
/// sender only; fan-out happens via the broadcast channel.
async fn handle_channel_message(
    state: &Arc<AppState>,
    id: &str,
    principal: &str,
    msg: &Json,
) -> Option<Json> {
    let kind = msg.get("type").and_then(Json::as_str).unwrap_or("");
    match kind {
        "hello" => welcome_message(state, id).await.ok().or(Some(json!({
            "type": "error", "error": "workbook is gone"
        }))),
        "cmd" => {
            let script = msg.get("script").and_then(Json::as_str).unwrap_or("");
            let cmd_id = msg.get("id").and_then(Json::as_str);
            if script.is_empty() {
                return Some(json!({ "type": "rejected", "cmd_id": cmd_id, "error": "cmd needs a script" }));
            }
            let mut tools = state.tools.lock().await;
            match tools.exec(id, script, principal, cmd_id) {
                // Fan-out (applied/rejected) already happened via the observer.
                Ok(_) => None,
                Err(e) => Some(json!({ "type": "rejected", "cmd_id": cmd_id, "error": e.0 })),
            }
        }
        "presence" => {
            let mut fanout = json!({
                "type": "presence",
                "principal": principal,
            });
            for key in ["selection", "viewport", "editing_cell"] {
                if let Some(v) = msg.get(key) {
                    fanout[key] = v.clone();
                }
            }
            state.channels.publish(id, fanout);
            None
        }
        "highlight.set" => {
            let range = msg.get("range").and_then(Json::as_str).unwrap_or("");
            let color = msg.get("color").and_then(Json::as_str).unwrap_or("amber");
            let note = msg.get("note").and_then(Json::as_str).map(String::from);
            let mut tools = state.tools.lock().await;
            let session = match tools.session(id) {
                Ok(s) => s,
                Err(e) => return Some(json!({ "type": "error", "error": e.0 })),
            };
            match session.resolve(range) {
                Ok(resolved) => {
                    let hid = session.add_highlight(
                        resolved.sheet_index,
                        &resolved.sheet_name,
                        resolved.range,
                        color,
                        note.clone(),
                        principal,
                    );
                    state.channels.publish(
                        id,
                        json!({
                            "type": "highlight.set",
                            "id": hid,
                            "range": resolved.qualified(),
                            "color": color,
                            "note": note,
                            "author": principal,
                        }),
                    );
                    None
                }
                Err(e) => Some(json!({ "type": "error", "error": e.0 })),
            }
        }
        "highlight.clear" => {
            let hid = msg.get("id").and_then(Json::as_u64).unwrap_or(0) as u32;
            let mut tools = state.tools.lock().await;
            let Ok(session) = tools.session(id) else {
                return Some(json!({ "type": "error", "error": "workbook is gone" }));
            };
            if session.remove_highlight(hid) {
                state.channels.publish(
                    id,
                    json!({ "type": "highlight.clear", "id": hid, "author": principal }),
                );
                None
            } else {
                Some(json!({ "type": "error", "error": format!("no highlight #{hid}") }))
            }
        }
        other => Some(json!({
            "type": "error",
            "error": format!("unknown message type {other:?} (hello, cmd, presence, highlight.set, highlight.clear)"),
        })),
    }
}
