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
