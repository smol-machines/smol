//! A synchronous client for the control plane.
//!
//! Blocking because the callers that needed this crate extracted were not
//! already async. An async caller can use [`crate::types`] and
//! [`crate::credentials`] with its own HTTP client.

use std::io::{BufRead, BufReader, Read};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::credentials::Credentials;
use crate::error::{Error, ErrorKind, Result};
use crate::types::{
    Agent, AgentPage, AgentTurn, AgentTurnAccepted, BranchBatch, Checkpoint, Command,
    CommandOutput, CreateAgent, CreateMachine, Machine, Port, SendAgentTurn, Share, Usage,
};

/// A recorded agent event or the terminal turn summary.
#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    /// One harness event, with the SSE id used to resume after disconnecting.
    Event {
        /// SSE id for reconnecting with `after`.
        id: u64,
        /// Harness event JSON.
        data: serde_json::Value,
    },
    /// The turn finished. This is the final stream item.
    Done(AgentTurn),
}

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
}

impl Client {
    /// Build a client for these credentials.
    pub fn new(credentials: Credentials) -> Result<Self> {
        let http = reqwest::blocking::Client::builder().build().map_err(|e| {
            Error::new(ErrorKind::Connection, format!("build the HTTP client: {e}"))
        })?;
        Ok(Self { credentials, http })
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
        let mut request = self
            .http
            .request(
                method.clone(),
                format!("{}{path}", self.credentials.base_url()),
            )
            .bearer_auth(self.credentials.api_key())
            .timeout(timeout);
        match body {
            Body::None => {}
            Body::Json(value) => request = request.json(&value),
            Body::Bytes(bytes) => {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(bytes.to_vec())
            }
        }
        let response = request.send().map_err(|e| {
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
        })?;
        check_status(response, &method, path)
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

    // -- managed agents ---------------------------------------------------

    /// Create a cloud managed agent session; setup continues in the background.
    pub fn create_agent(&self, request: &CreateAgent) -> Result<Agent> {
        self.json(
            reqwest::Method::POST,
            "/v1/agents",
            Body::Json(serde_json::to_value(request).map_err(serialize_error)?),
            REQUEST_TIMEOUT,
        )
    }

    /// Fetch a managed session and its turn history.
    pub fn agent(&self, name: &str) -> Result<Agent> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/agents/{}", agent_segment(name)),
            Body::None,
            REQUEST_TIMEOUT,
        )
    }

    /// List a page of session summaries.
    pub fn agents(&self, after: Option<&str>, limit: Option<u32>) -> Result<AgentPage> {
        let mut query = Vec::new();
        if let Some(after) = after {
            query.push(format!("after={}", encode_path(after)));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let path = if query.is_empty() {
            "/v1/agents".to_string()
        } else {
            format!("/v1/agents?{}", query.join("&"))
        };
        self.json(reqwest::Method::GET, &path, Body::None, REQUEST_TIMEOUT)
    }

    /// Submit a turn. Reusing an idempotency key with the same input returns its original index.
    pub fn send_agent_turn(
        &self,
        name: &str,
        request: &SendAgentTurn,
        idempotency_key: Option<&str>,
    ) -> Result<u64> {
        let path = format!("/v1/agents/{}/turns", agent_segment(name));
        let mut call = self
            .http
            .post(format!("{}{path}", self.credentials.base_url()))
            .bearer_auth(self.credentials.api_key())
            .timeout(REQUEST_TIMEOUT)
            .json(request);
        if let Some(key) = idempotency_key {
            call = call.header("Idempotency-Key", key);
        }
        let response = call.send().map_err(|error| {
            Error::new(
                if error.is_timeout() {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Connection
                },
                format!("POST {path} failed: {error}"),
            )
        })?;
        let response = check_status(response, &reqwest::Method::POST, &path)?;
        response
            .json::<AgentTurnAccepted>()
            .map(|accepted| accepted.turn)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Other,
                    format!("POST {path} returned an unreadable body: {error}"),
                )
            })
    }

    /// Replay and follow a turn's events; pass the last event id on reconnect.
    pub fn agent_events(&self, name: &str, turn: u64, after: Option<u64>) -> Result<AgentEvents> {
        let mut path = format!("/v1/agents/{}/turns/{turn}/events", agent_segment(name));
        if let Some(after) = after {
            path.push_str(&format!("?after={after}"));
        }
        let response = self
            .http
            .get(format!("{}{path}", self.credentials.base_url()))
            .bearer_auth(self.credentials.api_key())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .map_err(|error| {
                Error::new(ErrorKind::Connection, format!("GET {path} failed: {error}"))
            })?;
        Ok(AgentEvents::new(check_status(
            response,
            &reqwest::Method::GET,
            &path,
        )?))
    }

    /// Stop a running turn's machine and mark the turn cancelled.
    pub fn cancel_agent_turn(&self, name: &str, turn: u64) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/turns/{turn}/cancel", agent_segment(name)),
            Body::None,
            START_TIMEOUT,
        )
    }

    /// Restore the session to the state after `turn`.
    pub fn rewind_agent(&self, name: &str, turn: u64) -> Result<Agent> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/rewind", agent_segment(name)),
            Body::Json(serde_json::json!({"turn":turn})),
            START_TIMEOUT,
        )
    }

    /// Branch an independent session from a checkpointed turn.
    pub fn branch_agent(&self, name: &str, turn: u64, new_name: &str) -> Result<Agent> {
        let body = serde_json::json!({"turn":turn,"name":new_name});
        match self.json(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/branch", agent_segment(name)),
            Body::Json(body.clone()),
            START_TIMEOUT,
        ) {
            Err(error) if error.kind() == ErrorKind::NotFound => self.json(
                reqwest::Method::POST,
                &format!("/v1/agents/{}/fork", agent_segment(name)),
                Body::Json(body),
                START_TIMEOUT,
            ),
            result => result,
        }
    }

    /// Compatibility alias for the former agent operation.
    pub fn fork_agent(&self, name: &str, turn: u64, new_name: &str) -> Result<Agent> {
        self.json(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/fork", agent_segment(name)),
            Body::Json(serde_json::json!({"turn":turn,"name":new_name})),
            START_TIMEOUT,
        )
    }

    /// Pause an idle session's machine.
    pub fn pause_agent(&self, name: &str) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/pause", agent_segment(name)),
            Body::None,
            START_TIMEOUT,
        )
    }

    /// Resume an idle session's machine.
    pub fn resume_agent(&self, name: &str) -> Result<()> {
        self.empty(
            reqwest::Method::POST,
            &format!("/v1/agents/{}/resume", agent_segment(name)),
            Body::None,
            START_TIMEOUT,
        )
    }

    /// Delete a session and its unshared resources.
    pub fn delete_agent(&self, name: &str) -> Result<()> {
        self.empty(
            reqwest::Method::DELETE,
            &format!("/v1/agents/{}", agent_segment(name)),
            Body::None,
            START_TIMEOUT,
        )
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
        let path = format!("/v1/machines/{id}/exec/stream");
        let response = self
            .http
            .post(format!("{}{path}", self.credentials.base_url()))
            .bearer_auth(self.credentials.api_key())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(command)
            .send()
            .map_err(|e| Error::new(ErrorKind::Connection, format!("POST {path} failed: {e}")))?;
        let response = check_status(response, &reqwest::Method::POST, &path)?;
        Ok(SseEvents::new(response))
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
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let kind = ErrorKind::from_status(status.as_u16());
    let text = response.text().unwrap_or_default();
    let mut message = format!("{method} {path} → {status}");
    if !text.is_empty() {
        message.push_str(&format!(": {text}"));
    }
    Err(Error::new(kind, message).with_request_id(request_id))
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

fn agent_segment(name: &str) -> String {
    encode_path(name).replace('/', "%2F")
}

/// A resumable stream of one managed agent turn.
pub struct AgentEvents {
    reader: BufReader<Box<dyn Read + Send>>,
    finished: bool,
}

impl AgentEvents {
    fn new(response: impl Read + Send + 'static) -> Self {
        Self {
            reader: BufReader::new(Box::new(response)),
            finished: false,
        }
    }
}

impl Iterator for AgentEvents {
    type Item = Result<AgentStreamEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut kind = String::new();
        let mut id = 0;
        let mut data = Vec::new();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    self.finished = true;
                    return Some(Err(Error::new(
                        ErrorKind::Connection,
                        "agent event stream ended before the turn finished; reconnect with after",
                    )));
                }
                Ok(_) => {}
                Err(error) => {
                    self.finished = true;
                    return Some(Err(Error::new(
                        ErrorKind::Connection,
                        format!("agent event stream failed: {error}"),
                    )));
                }
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(value) = line.strip_prefix("event:") {
                kind = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("id:") {
                id = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim_start().to_string());
            } else if line.is_empty() {
                let payload = data.join("\n");
                match kind.as_str() {
                    "event" => {
                        return Some(
                            serde_json::from_str(&payload)
                                .map(|data| AgentStreamEvent::Event { id, data })
                                .map_err(|error| {
                                    Error::new(
                                        ErrorKind::Other,
                                        format!("invalid agent event: {error}"),
                                    )
                                }),
                        )
                    }
                    "done" => {
                        self.finished = true;
                        return Some(
                            serde_json::from_str(&payload)
                                .map(AgentStreamEvent::Done)
                                .map_err(|error| {
                                    Error::new(
                                        ErrorKind::Other,
                                        format!("invalid turn summary: {error}"),
                                    )
                                }),
                        );
                    }
                    "error" => {
                        self.finished = true;
                        return Some(Err(Error::new(ErrorKind::Other, payload)));
                    }
                    _ => {}
                }
                kind.clear();
                id = 0;
                data.clear();
            }
        }
    }
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
    use std::io::Write;

    #[test]
    fn managed_turn_sends_idempotency_key_and_camel_case_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    length = line
                        .split(':')
                        .nth(1)
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{\"turn\":0}").unwrap();
            (first, headers, body)
        });
        let client =
            Client::new(Credentials::new(format!("http://{address}"), "smk_test")).unwrap();
        let turn = client
            .send_agent_turn(
                "fixer",
                &SendAgentTurn {
                    prompt: "fix tests".into(),
                    env: Default::default(),
                    timeout_seconds: Some(123),
                },
                Some("task-1"),
            )
            .unwrap();
        assert_eq!(turn, 0);
        let (first, headers, body) = server.join().unwrap();
        assert!(first.starts_with("POST /v1/agents/fixer/turns HTTP/1.1"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("idempotency-key: task-1"));
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["timeoutSeconds"], 123);
    }

    #[test]
    fn managed_agent_stream_preserves_resume_id_and_terminal_turn() {
        let raw = concat!(
            "event: event\r\nid: 7\r\ndata: {\"type\":\"text\"}\r\n\r\n",
            "event: done\ndata: {\"index\":0,\"prompt\":\"fix\",\"status\":\"done\",",
            "\"isError\":false,\"checkpointed\":true,\"startedAt\":\"now\"}\n\n"
        );
        let mut stream = AgentEvents::new(std::io::Cursor::new(raw.as_bytes().to_vec()));
        match stream.next().unwrap().unwrap() {
            AgentStreamEvent::Event { id, data } => {
                assert_eq!(id, 7);
                assert_eq!(data["type"], "text");
            }
            _ => panic!("expected event"),
        }
        match stream.next().unwrap().unwrap() {
            AgentStreamEvent::Done(turn) => assert_eq!(turn.index, 0),
            _ => panic!("expected done"),
        }
        assert!(stream.next().is_none());
        let mut broken =
            AgentEvents::new(std::io::Cursor::new(b"event: event\ndata: {}\n\n".to_vec()));
        assert!(broken.next().unwrap().is_ok());
        assert_eq!(
            broken.next().unwrap().unwrap_err().kind(),
            ErrorKind::Connection
        );
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
