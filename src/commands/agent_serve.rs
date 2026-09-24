//! `smol agent serve`: agent sessions as an HTTP service.
//!
//! Turns run inside this service, not in the client that asked for them: a
//! client starts a turn and gets its number back, and the turn keeps running if
//! the client goes away. Every event a turn produces is appended to a file, so any
//! client can stream it — from the start or from where it left off — while the
//! turn runs and after it ends.
//!
//! ```text
//! POST   /v1/agents                          start a session
//! GET    /v1/agents                          list sessions
//! GET    /v1/agents/{name}                   a session and its running turn
//! POST   /v1/agents/{name}/turns             start a turn  -> 202 {"turn": n}
//! GET    /v1/agents/{name}/turns/{n}/events  the turn's events (SSE, ?after=K)
//! POST   /v1/agents/{name}/rewind            {"turn": n}
//! POST   /v1/agents/{name}/fork              {"turn": n, "name": "new"}
//! POST   /v1/agents/{name}/pause | /resume
//! DELETE /v1/agents/{name}
//! ```
//!
//! Sessions drive the local engine or smol cloud through the SDK, the same as
//! `smol agent`, and share its session records.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Args;
use serde::Deserialize;
use serde_json::{json, Value};
use smolmachines::agent::{sessions_dir, Harness, Session, SessionOptions};
use smolmachines::ConnectOptions;

#[derive(Args, Debug)]
pub struct AgentServeCmd {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1:7777")]
    listen: String,
    /// Bearer token clients must send (or set SMOL_AGENTS_TOKEN). Required when
    /// listening beyond loopback.
    #[arg(long)]
    token: Option<String>,
}

struct AppState {
    token: Option<String>,
    /// Sessions with a turn in flight, so a second turn waits its turn.
    busy: Mutex<HashSet<String>>,
    /// Per-session lock serializing record changes (turns, rewind, fork, delete).
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

type Shared = Arc<AppState>;

impl AppState {
    fn session_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<smolmachines::Error> for ApiError {
    fn from(e: smolmachines::Error) -> Self {
        let status = match e.kind() {
            smolmachines::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            smolmachines::ErrorKind::Config => StatusCode::BAD_REQUEST,
            smolmachines::ErrorKind::Conflict | smolmachines::ErrorKind::InvalidState => {
                StatusCode::CONFLICT
            }
            smolmachines::ErrorKind::NotSupported => StatusCode::UNPROCESSABLE_ENTITY,
            smolmachines::ErrorKind::Unauthorized => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(expected) = &state.token else {
        return Ok(());
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // Compare in constant time so the token cannot be recovered by timing.
    let ok = presented.is_some_and(|p| {
        p.len() == expected.len()
            && p.bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        ))
    }
}

/// Run blocking SDK work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Session names become path components, so reject anything but the SDK's
/// session-name alphabet before touching the filesystem.
fn check_name(name: &str) -> ApiResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("invalid session name '{name}'"),
        ))
    }
}

fn events_path(name: &str, turn: usize) -> ApiResult<PathBuf> {
    check_name(name)?;
    let dir = sessions_dir()?.join(name).join("events");
    Ok(dir.join(format!("turn-{turn}.jsonl")))
}

fn done_path(name: &str, turn: usize) -> ApiResult<PathBuf> {
    Ok(events_path(name, turn)?.with_extension("done"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    name: String,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    program: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cloud: bool,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    open_network: bool,
    #[serde(default)]
    checkpoints: Option<bool>,
    #[serde(default)]
    pause_between_turns: bool,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mib: Option<u32>,
}

async fn start_session(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<StartRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&state, &headers)?;
    let harness = match req.harness.as_deref().unwrap_or("claude-code") {
        "claude-code" => Harness::ClaudeCode,
        "command" => Harness::Command {
            image: req.image.clone().unwrap_or_else(|| "alpine:3.20".into()),
            program: req
                .program
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| vec!["sh".into(), "-c".into()]),
        },
        "codex" => Harness::Codex {
            model: req.model.clone(),
        },
        "opencode" => Harness::OpenCode {
            model: req.model.clone().ok_or_else(|| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    "the opencode harness needs a model, e.g. \"anthropic/claude-sonnet-4-5\""
                        .into(),
                )
            })?,
        },
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown harness '{other}' (claude-code, codex, opencode or command)"),
            ))
        }
    };
    let mut opts = SessionOptions::new(&req.name, harness);
    if req.cloud {
        opts.connect = ConnectOptions::cloud();
    }
    opts.extra_hosts = req.allow_hosts;
    opts.open_network = req.open_network;
    opts.checkpoint_turns = req.checkpoints.unwrap_or(true);
    opts.pause_between_turns = req.pause_between_turns;
    if let Some(cpus) = req.cpus {
        opts.cpus = cpus;
    }
    if let Some(mem) = req.memory_mib {
        opts.memory_mib = mem;
    }
    let lock = state.session_lock(&req.name);
    let record = blocking(move || {
        let _held = lock.lock().unwrap();
        Ok(Session::start(opts)?.record().clone())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!(record))))
}

async fn list_sessions(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let records = blocking(|| Ok(Session::list()?)).await?;
    Ok(Json(json!(records)))
}

async fn get_session(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let running = state.busy.lock().unwrap().contains(&name);
    let n = name.clone();
    let record = blocking(move || Ok(Session::open(&n)?.record().clone())).await?;
    let running_turn = running.then_some(record.turns.len());
    Ok(Json(
        json!({ "session": record, "runningTurn": running_turn }),
    ))
}

#[derive(Deserialize)]
struct TurnRequest {
    prompt: String,
    /// Environment for this turn only — typically the model API key.
    #[serde(default)]
    env: HashMap<String, String>,
}

async fn start_turn(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<TurnRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&state, &headers)?;
    if req.prompt.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "prompt is empty".into()));
    }
    let n = name.clone();
    let turn = blocking(move || Ok(Session::open(&n)?.record().turns.len())).await?;
    if !state.busy.lock().unwrap().insert(name.clone()) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("session '{name}' already has a turn running"),
        ));
    }
    let events = events_path(&name, turn)?;
    let done = done_path(&name, turn)?;
    if let Some(parent) = events.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let _ = std::fs::remove_file(&done);
    let file = std::fs::File::create(&events)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // The turn runs on its own thread and owns the session until it ends. The
    // caller only learns the turn number; the events file is the interface.
    let st = state.clone();
    let lock = state.session_lock(&name);
    let env: Vec<(String, String)> = req.env.into_iter().collect();
    let prompt = req.prompt;
    std::thread::spawn(move || {
        let outcome = {
            let _held = lock.lock().unwrap();
            let mut out = std::io::BufWriter::new(file);
            let result = Session::open(&name).and_then(|mut session| {
                session.send_with_env(&prompt, &env, &mut |event| {
                    if let Ok(line) = serde_json::to_string(event) {
                        let _ = writeln!(out, "{line}");
                        let _ = out.flush();
                    }
                })
            });
            match result {
                Ok(turn) => json!({ "ok": true, "turn": turn }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        };
        let _ = std::fs::write(&done, outcome.to_string());
        st.busy.lock().unwrap().remove(&name);
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "turn": turn }))))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: Option<usize>,
}

/// Stream a turn's events: everything after `?after=K` (event ids are line
/// numbers), then new ones as the turn produces them, then a final `done` event.
async fn turn_events(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path((name, turn)): Path<(String, usize)>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>> {
    authorize(&state, &headers)?;
    let events = events_path(&name, turn)?;
    let done = done_path(&name, turn)?;
    if !events.exists() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("session '{name}' has no turn {turn}"),
        ));
    }
    struct Cursor {
        events: PathBuf,
        done: PathBuf,
        next: usize,
        finished: bool,
        pending: std::collections::VecDeque<Event>,
    }
    let start = Cursor {
        events,
        done,
        next: q.after.map(|a| a + 1).unwrap_or(0),
        finished: false,
        pending: Default::default(),
    };
    let stream = futures_util::stream::unfold(start, |mut c| async move {
        loop {
            if let Some(ev) = c.pending.pop_front() {
                return Some((Ok(ev), c));
            }
            if c.finished {
                return None;
            }
            // Check `done` BEFORE reading, so the last events written before the
            // turn finished are always drained before the stream ends.
            let finished = c.done.exists();
            let text = tokio::fs::read_to_string(&c.events)
                .await
                .unwrap_or_default();
            for (i, line) in text.lines().enumerate().skip(c.next) {
                c.pending
                    .push_back(Event::default().id(i.to_string()).event("event").data(line));
                c.next = i + 1;
            }
            if finished {
                let outcome = tokio::fs::read_to_string(&c.done).await.unwrap_or_default();
                c.pending
                    .push_back(Event::default().event("done").data(outcome));
                c.finished = true;
            } else if c.pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn ensure_idle(state: &AppState, name: &str) -> ApiResult<()> {
    if state.busy.lock().unwrap().contains(name) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("session '{name}' has a turn running"),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct RewindRequest {
    turn: usize,
}

async fn rewind(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<RewindRequest>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    ensure_idle(&state, &name)?;
    let lock = state.session_lock(&name);
    let record = blocking(move || {
        let _held = lock.lock().unwrap();
        let mut session = Session::open(&name)?;
        session.rewind(req.turn)?;
        Ok(session.record().clone())
    })
    .await?;
    Ok(Json(json!(record)))
}

#[derive(Deserialize)]
struct ForkRequest {
    turn: usize,
    name: String,
}

async fn fork(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<ForkRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&state, &headers)?;
    let lock = state.session_lock(&name);
    let record = blocking(move || {
        let _held = lock.lock().unwrap();
        Ok(Session::open(&name)?
            .fork(req.turn, &req.name)?
            .record()
            .clone())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!(record))))
}

async fn pause(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    ensure_idle(&state, &name)?;
    blocking(move || Ok(Session::open(&name)?.pause()?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    blocking(move || Ok(Session::open(&name)?.resume()?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_session(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    ensure_idle(&state, &name)?;
    let lock = state.session_lock(&name);
    blocking(move || {
        let _held = lock.lock().unwrap();
        Ok(Session::open(&name)?.delete()?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router(token: Option<String>) -> Router {
    let state = Arc::new(AppState {
        token,
        busy: Mutex::new(HashSet::new()),
        locks: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route("/v1/agents", post(start_session).get(list_sessions))
        .route("/v1/agents/{name}", get(get_session).delete(delete_session))
        .route("/v1/agents/{name}/turns", post(start_turn))
        .route("/v1/agents/{name}/turns/{turn}/events", get(turn_events))
        .route("/v1/agents/{name}/rewind", post(rewind))
        .route("/v1/agents/{name}/fork", post(fork))
        .route("/v1/agents/{name}/pause", post(pause))
        .route("/v1/agents/{name}/resume", post(resume))
        .with_state(state)
}

impl AgentServeCmd {
    pub fn run(self) -> Result<()> {
        let addr: std::net::SocketAddr = self.listen.parse()?;
        let token = self
            .token
            .or_else(|| std::env::var("SMOL_AGENTS_TOKEN").ok())
            .filter(|t| !t.is_empty());
        if !addr.ip().is_loopback() && token.is_none() {
            bail!("refusing to listen on {addr} without --token (or SMOL_AGENTS_TOKEN)");
        }
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            eprintln!("smol agent service listening on http://{addr}");
            axum::serve(listener, router(token)).await?;
            Ok(())
        })
    }
}
