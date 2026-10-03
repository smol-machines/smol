//! A synchronous client for the control plane.
//!
//! Blocking because the callers that needed this crate extracted were not
//! already async. An async caller can use [`crate::types`] and
//! [`crate::credentials`] with its own HTTP client.

use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::credentials::Credentials;
use crate::error::{Error, ErrorKind, Result};
use crate::types::{
    BranchBatch, Checkpoint, Command, CommandOutput, CreateMachine, Machine, Port, Share, Usage,
};

/// Ordinary calls are short: a hung request must not block a caller forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Starting can include a cold image pull, so it gets its own long window.
pub const START_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Capture can take minutes on a large machine.
pub const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Grace before the agent probe stands in for `ready`, so the flag keeps first
/// refusal and the probe never preempts a machine about to flip.
const NO_PORT_PROBE_GRACE: Duration = Duration::from_secs(2);

/// One event from a running command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A chunk of stdout.
    Stdout(String),
    /// A chunk of stderr.
    Stderr(String),
    /// The command exited. Always last.
    Exit(i32),
    /// The stream itself failed.
    Error(String),
}

/// What a request carries, if anything.
enum Body<'a> {
    None,
    Json(serde_json::Value),
    Bytes(&'a [u8]),
}

/// A client for one control plane.
#[derive(Debug, Clone)]
pub struct Client {
    credentials: Credentials,
    http: reqwest::blocking::Client,
    /// Streams only: async so a read can be raced against a cancel.
    stream_http: reqwest::Client,
}

impl Client {
    /// Build a client for these credentials.
    pub fn new(credentials: Credentials) -> Result<Self> {
        let http = reqwest::blocking::Client::builder().build().map_err(|e| {
            Error::new(ErrorKind::Connection, format!("build the HTTP client: {e}"))
        })?;
        // Each stream runs on its own short-lived runtime, and a pooled
        // connection is bound to the runtime that opened it, so never reuse one.
        let stream_http = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Connection, format!("build the HTTP client: {e}"))
            })?;
        Ok(Self {
            credentials,
            http,
            stream_http,
        })
    }

    /// Build a client, resolving credentials from the environment and the CLI
    /// session.
    pub fn resolve(base_url: Option<String>, api_key: Option<String>) -> Result<Self> {
        Self::new(Credentials::resolve(base_url, api_key)?)
    }

    /// The credentials this client uses.
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    // -- plumbing ----------------------------------------------------------

    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body<'_>,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response> {
        self.credentials.renew_if_expiring()?;
        let key = self.credentials.api_key();
        let response = self.send_once(&method, path, &body, timeout, &key)?;
        // A CLI session the control plane stopped honouring before its
        // recorded expiry gets one renewal and one retry; any other 401 is a
        // verdict.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && self.credentials.renew_after_rejection(&key)?
        {
            let key = self.credentials.api_key();
            let response = self.send_once(&method, path, &body, timeout, &key)?;
            return check_status(response, &method, path);
        }
        check_status(response, &method, path)
    }

    fn send_once(
        &self,
        method: &reqwest::Method,
        path: &str,
        body: &Body<'_>,
        timeout: Duration,
        key: &str,
    ) -> Result<reqwest::blocking::Response> {
        let mut request = self
            .http
            .request(
                method.clone(),
                format!("{}{path}", self.credentials.base_url()),
            )
            .bearer_auth(key)
            .timeout(timeout);
        match body {
            Body::None => {}
            Body::Json(value) => request = request.json(value),
            Body::Bytes(bytes) => {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(bytes.to_vec())
            }
        }
        request.send().map_err(|e| {
            if e.is_timeout() {
                Error::new(
                    ErrorKind::Timeout,
                    format!("{method} {path} timed out after {timeout:?}"),
                )
            } else {
                Error::new(
                    ErrorKind::Connection,
                    format!("{method} {path} failed: {e}"),
                )
            }
        })
    }

    fn json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body<'_>,
        timeout: Duration,
    ) -> Result<T> {
        let response = self.send(method.clone(), path, body, timeout)?;
        response.json::<T>().map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("{method} {path} returned an unreadable body: {e}"),
            )
        })
    }

    fn empty(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body<'_>,
        timeout: Duration,
    ) -> Result<()> {
        self.send(method, path, body, timeout).map(|_| ())
    }

    // -- machines ----------------------------------------------------------

    /// Fetch one machine.
    pub fn machine(&self, id: &str) -> Result<Machine> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/machines/{id}"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Every machine in the account.
    pub fn machines(&self) -> Result<Vec<Machine>> {
        self.json(
            reqwest::Method::GET,
            "/v1/machines",
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Create a machine. It is not started.
    pub fn create_machine(&self, request: &CreateMachine) -> Result<Machine> {
        self.json(
            reqwest::Method::POST,
            "/v1/machines",
            Body::Json(serde_json::to_value(request).map_err(serialize_error)?),
            REQUEST_TIMEOUT,
        )
    }

    /// Start a machine. `branchable` asks for cloneable guest RAM.
    pub fn start(&self, id: &str, branchable: bool) -> Result<()> {
        let path = if branchable {
            format!("/v1/machines/{id}/start?forkable=true")
        } else {
            format!("/v1/machines/{id}/start")
        };
        self.empty(reqwest::Method::POST, &path, Body::None, START_TIMEOUT)
    }

    /// Stop a machine, keeping its disks.
    pub fn stop(&self, id: &str) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/stop"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Save execution durably and stop the machine.
    pub fn pause(&self, id: &str) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/pause"),
            Body::None,
            START_TIMEOUT,
        )
    }

    /// Resume saved execution in the same machine.
    pub fn resume(&self, id: &str) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/resume"),
            Body::None,
            START_TIMEOUT,
        )
    }

    /// Delete a machine.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.empty(
            reqwest::Method::DELETE,
            &format!("/v1/machines/{id}"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Resolve a machine name to its id, since the API addresses machines by
    /// id and people address them by name.
    pub fn resolve_id(&self, name_or_id: &str) -> Result<String> {
        match self.machine(name_or_id) {
            Ok(_) => return Ok(name_or_id.to_string()),
            // Only a genuine miss is worth a name search. Reporting an expired
            // key or an unreachable control plane as "no such machine" sends
            // the caller looking for the wrong problem.
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(error),
            Err(_) => {}
        }
        self.machines()?
            .into_iter()
            .find(|machine| machine.name.as_deref() == Some(name_or_id))
            .map(|machine| machine.id)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("no machine named {name_or_id}"),
                )
            })
    }

    /// Wait until a machine is ready to do work.
    ///
    /// Readiness is later than `started`: a started machine is still booting,
    /// and acting then is the race that passes on a slow cold start and fails
    /// on a warm one. A machine with no published port never flips `ready` on
    /// control planes that gate it on a port accepting, so after a grace this
    /// falls back to asking the guest agent directly.
    pub fn wait_until_ready(&self, id: &str, timeout: Duration, interval: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            let machine = match self.machine(id) {
                Ok(machine) => Some(machine),
                // Auth and not-found are verdicts, not weather.
                Err(error)
                    if matches!(error.kind(), ErrorKind::Unauthorized | ErrorKind::NotFound) =>
                {
                    return Err(error)
                }
                Err(_) => None,
            };

            if let Some(machine) = &machine {
                if machine.ready == Some(true) {
                    return Ok(());
                }
                if machine.is_terminal() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "machine {id} entered {} before becoming ready",
                            machine.state
                        ),
                    ));
                }
                // An older control plane omits `ready` entirely.
                if machine.ready.is_none() && machine.is_started() {
                    return Ok(());
                }
                // A machine with no published port never flips `ready` on a
                // control plane that gates readiness on a port accepting a
                // connection. Gate this on having *started*, not on readiness:
                // readiness is the thing that is stuck.
                if machine.is_started()
                    && machine.ports.is_empty()
                    && start.elapsed() >= NO_PORT_PROBE_GRACE
                    && self.agent_reachable(id)
                {
                    return Ok(());
                }
            }

            if start.elapsed() >= timeout {
                let state = machine
                    .map(|machine| machine.state)
                    .unwrap_or_else(|| "unknown".into());
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("machine {id} not ready after {timeout:?} (state={state})"),
                ));
            }
            std::thread::sleep(interval);
        }
    }

    /// A trivial in-guest command that only succeeds once the agent answers.
    fn agent_reachable(&self, id: &str) -> bool {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/exec"),
            Body::Json(serde_json::json!({ "command": ["sh", "-c", "true"] })),
            Duration::from_secs(15),
        )
        .is_ok()
    }

    // -- work --------------------------------------------------------------

    /// Run a command and wait for it.
    pub fn exec(&self, id: &str, command: &Command, timeout: Duration) -> Result<CommandOutput> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/exec"),
            Body::Json(serde_json::to_value(command).map_err(serialize_error)?),
            timeout,
        )
    }

    /// Run a command and read its output as it arrives.
    ///
    /// The returned iterator ends when the command exits, after the final
    /// [`StreamEvent::Exit`].
    pub fn exec_stream(
        &self,
        id: &str,
        command: &Command,
    ) -> Result<impl Iterator<Item = StreamEvent>> {
        self.exec_stream_cancellable(id, command, &StreamCancel::new())
    }

    /// [`Self::exec_stream`], ended early by `cancel` from any thread.
    ///
    /// A command can run, and stay quiet, for longer than any fixed request
    /// window, so only the wait for the response headers is bounded (by
    /// [`REQUEST_TIMEOUT`]); the body is read for as long as the command runs.
    /// Cancelling closes the connection at once, even while a reader is blocked
    /// waiting for the next event, and the iterator then ends. Whether the
    /// command keeps running is up to the control plane.
    pub fn exec_stream_cancellable(
        &self,
        id: &str,
        command: &Command,
        cancel: &StreamCancel,
    ) -> Result<impl Iterator<Item = StreamEvent>> {
        let path = format!("/v1/machines/{id}/exec/stream");
        // A private runtime per stream: the blocking client's reads cannot be
        // interrupted from another thread, and an async read raced against the
        // cancel token can.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(ErrorKind::Other, format!("start a stream runtime: {e}")))?;
        self.credentials.renew_if_expiring()?;
        let request = self
            .stream_http
            .post(format!("{}{path}", self.credentials.base_url()))
            .bearer_auth(self.credentials.api_key())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(command);
        let mut cancelled = cancel.subscribe();
        let sent = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = wait_cancelled(&mut cancelled) => None,
                sent = tokio::time::timeout(REQUEST_TIMEOUT, request.send()) => Some(sent),
            }
        });
        let response = match sent {
            None => None,
            Some(Err(_)) => {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("POST {path} timed out after {REQUEST_TIMEOUT:?}"),
                ))
            }
            Some(Ok(Err(e))) => {
                return Err(Error::new(
                    ErrorKind::Connection,
                    format!("POST {path} failed: {e}"),
                ))
            }
            Some(Ok(Ok(response))) if response.status().is_success() => Some(response),
            Some(Ok(Ok(response))) => {
                let status = response.status();
                let request_id = request_id(response.headers());
                let text = runtime
                    .block_on(async {
                        tokio::time::timeout(REQUEST_TIMEOUT, response.text()).await
                    })
                    .ok()
                    .and_then(|text| text.ok())
                    .unwrap_or_default();
                return Err(status_error(status, &reqwest::Method::POST, &path, &text)
                    .with_request_id(request_id));
            }
        };
        Ok(SseEvents::new(StreamBody {
            runtime,
            response,
            cancelled,
            pending: Vec::new(),
            offset: 0,
        }))
    }

    /// Read a file out of a machine.
    pub fn read_file(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        let response = self.send(
            reqwest::Method::GET,
            &format!("/v1/machines/{id}/files/{}", encode_path(path)),
            Body::None,
            REQUEST_TIMEOUT,
        )?;
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|e| Error::new(ErrorKind::Connection, format!("read {path}: {e}")))
    }

    /// Write a file into a machine. The route carries no mode.
    pub fn write_file(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.empty(
            reqwest::Method::PUT,
            &format!("/v1/machines/{id}/files/{}", encode_path(path)),
            Body::Bytes(data),
            REQUEST_TIMEOUT,
        )
    }

    // -- branches and checkpoints -----------------------------------------

    /// Branch a machine into one live copy-on-write clone.
    pub fn branch(
        &self,
        id: &str,
        name: &str,
        ports: &[Port],
        branchable: bool,
    ) -> Result<Machine> {
        let ports = port_bodies(ports);
        let mut branch_body = serde_json::json!({ "name": name, "ports": ports });
        let mut fork_body = branch_body.clone();
        if branchable {
            branch_body["branchable"] = true.into();
            fork_body["forkable"] = true.into();
        }
        self.branch_call(
            &format!("/v1/machines/{id}/branches"),
            branch_body,
            &format!("/v1/machines/{id}/fork"),
            fork_body,
        )
    }

    /// Branch a machine into many clones in one transactional call: all of
    /// them, or none.
    pub fn branch_batch(&self, id: &str, names: &[String], ports: &[Port]) -> Result<Vec<Machine>> {
        let body = serde_json::json!({ "names": names, "ports": port_bodies(ports) });
        let batch: BranchBatch = self.branch_call(
            &format!("/v1/machines/{id}/branches/batch"),
            body.clone(),
            &format!("/v1/machines/{id}/fork-batch"),
            body,
        )?;
        Ok(batch.clones)
    }

    /// Try the branch route, falling back to the older fork route.
    ///
    /// The control plane is mid-rollout from fork to branch vocabulary, and an
    /// older one answers 404 on the new path.
    fn branch_call<T: DeserializeOwned>(
        &self,
        branch_path: &str,
        branch_body: serde_json::Value,
        fork_path: &str,
        fork_body: serde_json::Value,
    ) -> Result<T> {
        match self.json::<T>(
            reqwest::Method::POST,
            branch_path,
            Body::Json(branch_body),
            START_TIMEOUT,
        ) {
            Err(error) if error.kind() == ErrorKind::NotFound => self.json(
                reqwest::Method::POST,
                fork_path,
                Body::Json(fork_body),
                START_TIMEOUT,
            ),
            other => other,
        }
    }

    /// Capture a machine. The control plane stores the artifact.
    pub fn checkpoint(&self, id: &str) -> Result<Checkpoint> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/checkpoints"),
            Body::None,
            CHECKPOINT_TIMEOUT,
        )
    }

    /// Create a machine from a stored capture.
    ///
    /// The new machine comes back stopped; start it and wait for readiness the
    /// way [`Client::start`] and [`Client::wait_until_ready`] do. A capture
    /// only restores on the architecture it was taken on.
    pub fn restore_checkpoint(&self, checkpoint_id: &str, name: &str) -> Result<Machine> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/checkpoints/{}/restore", encode_path(checkpoint_id)),
            Body::Json(serde_json::json!({ "name": name })),
            CHECKPOINT_TIMEOUT,
        )
    }

    /// Delete a machine and take a final, settled usage reading.
    ///
    /// The control plane samples usage synchronously before the teardown, so
    /// the report is complete — no waiting for the periodic metering rollup.
    pub fn delete_with_usage(&self, id: &str) -> Result<Usage> {
        self.json(
            reqwest::Method::DELETE,
            &format!("/v1/machines/{id}?includeUsage=true"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Every stored capture of a machine.
    pub fn checkpoints(&self, id: &str) -> Result<Vec<Checkpoint>> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/machines/{id}/checkpoints"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    // -- account -----------------------------------------------------------

    /// Metered usage and cost for a machine.
    pub fn usage(&self, id: &str) -> Result<Usage> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/machines/{id}/usage"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Publish a shareable link.
    pub fn share(&self, id: &str) -> Result<Share> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/machines/{id}/share"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// Withdraw a shareable link.
    pub fn unshare(&self, id: &str) -> Result<()> {
        self.empty(
            reqwest::Method::DELETE,
            &format!("/v1/machines/{id}/share"),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// The authenticated bridge URL for a published guest port.
    ///
    /// A bare path must stay `connect/<port>` with no trailing slash — that is
    /// the route that exists; `connect/<port>/` matches nothing and 404s.
    pub fn connect_url(&self, id: &str, port: u16, path: &str) -> String {
        let suffix = path.trim_start_matches('/');
        let base = self.credentials.base_url();
        if suffix.is_empty() {
            format!("{base}/v1/machines/{id}/connect/{port}")
        } else {
            format!("{base}/v1/machines/{id}/connect/{port}/{suffix}")
        }
    }
}

fn serialize_error(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Other, format!("encode the request body: {e}"))
}

fn port_bodies(ports: &[Port]) -> Vec<serde_json::Value> {
    ports
        .iter()
        .map(|port| serde_json::json!({ "port": port.port, "hostPort": port.host_port }))
        .collect()
}

/// Turn a non-2xx response into an error naming the status, the server's
/// explanation and the correlation id support will ask for.
fn check_status(
    response: reqwest::blocking::Response,
    method: &reqwest::Method,
    path: &str,
) -> Result<reqwest::blocking::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let request_id = request_id(response.headers());
    let text = response.text().unwrap_or_default();
    Err(status_error(status, method, path, &text).with_request_id(request_id))
}

fn request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn status_error(
    status: reqwest::StatusCode,
    method: &reqwest::Method,
    path: &str,
    text: &str,
) -> Error {
    let mut message = format!("{method} {path} → {status}");
    if !text.is_empty() {
        message.push_str(&format!(": {text}"));
    }
    Error::new(ErrorKind::from_status(status.as_u16()), message)
}

/// Ends a live [`Client::exec_stream_cancellable`] stream from any thread.
/// Clones share one token; cancelling twice is harmless.
#[derive(Debug, Clone)]
pub struct StreamCancel(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for StreamCancel {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl StreamCancel {
    /// A token that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// End the stream: close its connection, or keep it from being opened.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether [`Self::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Resolves once the token is cancelled.
async fn wait_cancelled(cancelled: &mut tokio::sync::watch::Receiver<bool>) {
    // The stream holds a receiver, not the sender, so the sender can go away
    // with the caller's token; that is not a cancel, so wait forever then.
    if cancelled.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// A streaming response body, read chunk by chunk on the stream's runtime and
/// abandoned the moment its token is cancelled.
struct StreamBody {
    runtime: tokio::runtime::Runtime,
    /// None once the body ended or was cancelled; dropping it closes the
    /// connection.
    response: Option<reqwest::Response>,
    cancelled: tokio::sync::watch::Receiver<bool>,
    pending: Vec<u8>,
    /// How much of `pending` has been handed out.
    offset: usize,
}

impl Read for StreamBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.offset == self.pending.len() {
            let Some(response) = self.response.as_mut() else {
                return Ok(0);
            };
            let cancelled = &mut self.cancelled;
            let next = self.runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = wait_cancelled(cancelled) => None,
                    chunk = response.chunk() => Some(chunk),
                }
            });
            match next {
                Some(Ok(Some(chunk))) => {
                    self.pending = chunk.to_vec();
                    self.offset = 0;
                }
                Some(Err(e)) => {
                    self.response = None;
                    return Err(std::io::Error::other(e));
                }
                // The body ended, or the caller cancelled: either way close the
                // connection now rather than when the reader is dropped.
                Some(Ok(None)) | None => {
                    self.response = None;
                    return Ok(0);
                }
            }
        }
        let rest = &self.pending[self.offset..];
        let n = buf.len().min(rest.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.offset += n;
        Ok(n)
    }
}

/// Percent-encode each path segment but keep the separators: the files route is
/// a wildcard, so slashes are structural while spaces and `?`, `#`, `%` in a
/// filename are not.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (byte as char).to_string()
                    }
                    other => format!("%{other:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Server-sent events, parsed off a reader.
///
/// Each event is an `event:` line naming the kind and one or more `data:` lines
/// carrying the payload, ended by a blank line.
struct SseEvents<R: Read> {
    reader: BufReader<R>,
    kind: String,
    data: Vec<String>,
    finished: bool,
}

impl<R: Read> SseEvents<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            kind: String::new(),
            data: Vec::new(),
            finished: false,
        }
    }

    fn take_event(&mut self) -> Option<StreamEvent> {
        let event = sse_event(&self.kind, &self.data.join("\n"));
        self.kind.clear();
        self.data.clear();
        event
    }
}

impl<R: Read> Iterator for SseEvents<R> {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    self.finished = true;
                    // A stream that ended mid-event still owes that event.
                    return self.take_event();
                }
                Ok(_) => {}
                Err(error) => {
                    self.finished = true;
                    return Some(StreamEvent::Error(error.to_string()));
                }
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if let Some(event) = self.take_event() {
                    return Some(event);
                }
            } else if let Some(rest) = trimmed.strip_prefix("event:") {
                self.kind = rest.trim().to_string();
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                self.data
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
    }
}

fn sse_event(kind: &str, data: &str) -> Option<StreamEvent> {
    match kind {
        "stdout" => Some(StreamEvent::Stdout(data.to_string())),
        "stderr" => Some(StreamEvent::Stderr(data.to_string())),
        "error" => Some(StreamEvent::Error(data.to_string())),
        "exit" => Some(StreamEvent::Exit(
            serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|value| value.get("exitCode").and_then(serde_json::Value::as_i64))
                .unwrap_or(0) as i32,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One server standing in for both the control plane and the identity
    /// provider: it rejects `jwt-old`, accepts `jwt-new`, and hands out
    /// `jwt-new` for a refresh. Returns its URL and how many 401s it sent.
    fn rejecting_then_renewing_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>)
    {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let rejected = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = rejected.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = vec![0_u8; 8192];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_lowercase();
                let (status, body) = if request.starts_with("post /oauth/token") {
                    (
                        "200 OK",
                        r#"{"access_token":"jwt-new","refresh_token":"rt-new","expires_in":86400}"#,
                    )
                } else if request.contains("authorization: bearer jwt-new") {
                    ("200 OK", "[]")
                } else {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ("401 Unauthorized", r#"{"error":"unauthorized"}"#)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, rejected)
    }

    #[test]
    fn a_rejected_cli_session_is_renewed_and_the_request_retried_once() {
        let (url, rejected) = rejecting_then_renewing_server();
        let dir = std::env::temp_dir().join(format!("smol-cloud-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[cloud]\napi_key = \"jwt-old\"\nrefresh_token = \"rt-old\"\n",
        )
        .unwrap();
        let client = Client::new(Credentials::test_session(
            &url, "jwt-old", "rt-old", &config, &url,
        ))
        .unwrap();

        assert!(client.machines().unwrap().is_empty());
        assert_eq!(rejected.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(client.credentials().api_key(), "jwt-new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rejected_api_key_is_not_retried() {
        let (url, rejected) = rejecting_then_renewing_server();
        let client = Client::new(Credentials::new(&url, "smk_revoked")).unwrap();

        let error = client.machines().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unauthorized);
        assert_eq!(rejected.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_files_path_keeps_its_separators_and_escapes_the_rest() {
        assert_eq!(encode_path("/tmp/a b.txt"), "/tmp/a%20b.txt");
        assert_eq!(encode_path("/var/log/x?y"), "/var/log/x%3Fy");
        assert_eq!(encode_path("/plain/path"), "/plain/path");
    }

    #[test]
    fn an_exit_event_carries_its_code_out_of_the_json_payload() {
        assert_eq!(
            sse_event("exit", r#"{"exitCode":7}"#),
            Some(StreamEvent::Exit(7))
        );
        // A malformed payload must not lose the exit event itself.
        assert_eq!(sse_event("exit", "not json"), Some(StreamEvent::Exit(0)));
        assert_eq!(sse_event("unknown", "x"), None);
    }

    #[test]
    fn a_stream_is_parsed_event_by_event() {
        let raw = "event: stdout\ndata: hello\n\nevent: stderr\ndata: oops\n\nevent: exit\ndata: {\"exitCode\":3}\n\n";
        let events: Vec<_> = SseEvents::new(raw.as_bytes()).collect();
        assert_eq!(
            events,
            vec![
                StreamEvent::Stdout("hello".into()),
                StreamEvent::Stderr("oops".into()),
                StreamEvent::Exit(3),
            ]
        );
    }

    #[test]
    fn multiple_data_lines_join_the_way_the_spec_says() {
        let raw = "event: stdout\ndata: one\ndata: two\n\n";
        let events: Vec<_> = SseEvents::new(raw.as_bytes()).collect();
        assert_eq!(events, vec![StreamEvent::Stdout("one\ntwo".into())]);
    }

    #[test]
    fn a_stream_cut_short_still_yields_its_last_event() {
        // No trailing blank line: the server died mid-event.
        let raw = "event: stdout\ndata: partial\n";
        let events: Vec<_> = SseEvents::new(raw.as_bytes()).collect();
        assert_eq!(events, vec![StreamEvent::Stdout("partial".into())]);
    }
}
