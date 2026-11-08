//! Resumable execs, without the HTTP: what to send and how to read the answers.
//!
//! A resumable exec (`/v1/machines/{id}/execs`) is a command the machine's node
//! owns rather than the connection that started it. The client names it before
//! the first try (so a retried start is the same start), then reads its output
//! by offset: `GET /v1/machines/{id}/execs/{exec}?after=<next>` returns the
//! output after `next`, and asking with `after = next` also tells the node it
//! may drop everything before it. A connection that drops, or a control that
//! restarts, costs nothing but a retry: the same read again returns the same
//! bytes, so a client that only ever advances to the `next` it was given sees
//! each byte once, and the exit code at the end.
//!
//! This module is the protocol alone; [`crate::blocking`] and the `smol` CLI
//! drive it with their own HTTP clients.

use std::time::{Duration, Instant};

use base64::Engine as _;

/// Seconds a read asks the control to wait for output when there is none yet.
pub const READ_WAIT_SECS: u64 = 20;

/// How long one read may take on the wire: its wait plus room for the answer.
pub const READ_TIMEOUT: Duration = Duration::from_secs(READ_WAIT_SECS + 30);

/// How long a client keeps retrying a read that fails (the control
/// restarting, the network gone) before it gives up on the exec. The command
/// itself keeps running on the node meanwhile.
pub const RECONNECT_BUDGET: Duration = Duration::from_secs(10 * 60);

/// A fresh exec id: 32 hex digits, unique per call.
pub fn new_exec_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    // RandomState is seeded from the OS once per process; two of them give two
    // independent 64-bit halves.
    let half = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u64(count);
        h.write_u32(std::process::id());
        h.write_u64(salt);
        h.finish()
    };
    format!("{:016x}{:016x}", half(1), half(2))
}

/// The body of a start: the command's fields plus the exec's id and how long
/// to wait for its first output.
pub fn start_body(mut command: serde_json::Value, exec_id: &str) -> serde_json::Value {
    if let Some(map) = command.as_object_mut() {
        map.remove("stream");
        map.remove("background");
        map.insert("execId".into(), exec_id.into());
        map.insert("waitSeconds".into(), READ_WAIT_SECS.into());
    }
    command
}

/// Whether a start's answer is an exec's. A catch-all route that answers
/// anything with 200 is a control plane without resumable execs.
pub fn is_exec_answer(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .is_some_and(|v| v.get("execId").is_some_and(|id| id.is_string()))
}

/// The path that starts an exec in machine `machine`.
pub fn start_path(machine: &str) -> String {
    format!("/v1/machines/{machine}/execs")
}

/// One piece of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Bytes the command wrote to stdout.
    Stdout(Vec<u8>),
    /// Bytes the command wrote to stderr.
    Stderr(Vec<u8>),
}

/// How an exec ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The command's exit code.
    pub exit_code: Option<i32>,
    /// Why there is no exit code (killed, its machine stopped).
    pub error: Option<String>,
}

/// What one answer carried.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Absorbed {
    /// Output not seen before, in order.
    pub pieces: Vec<Piece>,
    /// Bytes `from..to` were dropped by the node before this client read them:
    /// it was away longer than the node holds unread output.
    pub lost: Option<(u64, u64)>,
    /// Set once the exec ended and all its output was read.
    pub done: Option<Outcome>,
}

/// Follows one exec's output across any number of answers, delivering each
/// byte once.
#[derive(Debug, Clone)]
pub struct Reader {
    machine: String,
    exec_id: String,
    next: u64,
}

impl Reader {
    /// Follow exec `exec_id` in machine `machine` from its first byte.
    pub fn new(machine: &str, exec_id: &str) -> Self {
        Self {
            machine: machine.to_string(),
            exec_id: exec_id.to_string(),
            next: 0,
        }
    }

    /// The exec's id.
    pub fn exec_id(&self) -> &str {
        &self.exec_id
    }

    /// The offset of the next byte to read.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// The path of the next read.
    pub fn read_path(&self) -> String {
        format!(
            "/v1/machines/{}/execs/{}?after={}&wait={READ_WAIT_SECS}",
            self.machine, self.exec_id, self.next
        )
    }

    /// The path that deletes the exec (once its end was read).
    pub fn delete_path(&self) -> String {
        format!("/v1/machines/{}/execs/{}", self.machine, self.exec_id)
    }

    /// Take one answer (from the start or a read): output past what was
    /// already seen, and the end once reached. Repeated or overlapping output
    /// is skipped, so an answer received twice changes nothing.
    pub fn absorb(&mut self, body: &[u8]) -> Result<Absorbed, String> {
        let v: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| format!("unreadable answer from the control plane: {e}"))?;
        if v.get("next").and_then(|n| n.as_u64()).is_none() {
            return Err("the control plane answered with something that is not an exec".into());
        }
        let mut out = Absorbed::default();
        if let (Some(from), Some(to)) = (
            v.pointer("/lost/from").and_then(|x| x.as_u64()),
            v.pointer("/lost/to").and_then(|x| x.as_u64()),
        ) {
            if to > self.next {
                out.lost = Some((from.max(self.next), to));
                self.next = to;
            }
        }
        for chunk in v
            .get("output")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
        {
            let offset = chunk.get("offset").and_then(|o| o.as_u64()).unwrap_or(0);
            let data = chunk
                .get("data")
                .and_then(|d| d.as_str())
                .map(|d| base64::engine::general_purpose::STANDARD.decode(d))
                .transpose()
                .map_err(|e| format!("undecodable output from the control plane: {e}"))?
                .unwrap_or_default();
            let end = offset + data.len() as u64;
            if end <= self.next {
                continue;
            }
            if offset > self.next {
                // A hole the node did not report as lost: never deliver bytes
                // out of order; read again from where this client is.
                break;
            }
            let fresh = data[(self.next - offset) as usize..].to_vec();
            self.next = end;
            let stream = chunk.get("stream").and_then(|s| s.as_str());
            out.pieces.push(if stream == Some("stderr") {
                Piece::Stderr(fresh)
            } else {
                Piece::Stdout(fresh)
            });
        }
        let reached = v
            .get("next")
            .and_then(|n| n.as_u64())
            .is_some_and(|n| n <= self.next);
        if v.get("done").and_then(|d| d.as_bool()) == Some(true) && reached {
            out.done = Some(Outcome {
                exit_code: v.get("exitCode").and_then(|c| c.as_i64()).map(|c| c as i32),
                error: v.get("error").and_then(|e| e.as_str()).map(str::to_string),
            });
        }
        Ok(out)
    }
}

/// What to do about an answer that is not a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Send the same request again, after the server's `Retry-After` if any.
    Retry,
    /// The control plane (or the machine's node) has no resumable execs: use
    /// the plain exec instead. Only ever said of a start.
    Unsupported,
    /// A real refusal: report it.
    Fail,
}

/// Classify a status and the problem `code` its body carried, if any.
/// `starting` is true for the start request.
///
/// A read is safe to repeat whatever happened, so every transient refusal is
/// retried. A start is idempotent on its exec id while the node keeps the
/// exec, so it is retried only when the request cannot have reached a node:
/// a gateway error, a draining control, a rate limit. A refusal that says the
/// machine went away (`machine_interrupted`) or is out of room is reported,
/// as the plain exec reports it.
pub fn verdict(status: u16, code: Option<&str>, starting: bool) -> Verdict {
    match status {
        408 | 429 | 502 | 504 => Verdict::Retry,
        503 if code == Some("machine_interrupted") => Verdict::Fail,
        503 if !starting => Verdict::Retry,
        503 if matches!(code, None | Some("control_draining")) => Verdict::Retry,
        // A control without the route answers 404 (405 behind some proxies);
        // a control whose node has no resumable execs answers 501. A start
        // that falls back on a 404 for a machine that really is missing gets
        // the same 404 from the plain exec, so nothing is hidden.
        404 | 405 | 501 if starting => Verdict::Unsupported,
        _ => Verdict::Fail,
    }
}

/// The problem `code` of an error body, if it has one.
pub fn problem_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("code")?
        .as_str()
        .map(str::to_string)
}

/// Waits between retries of one request: short at first (a control restart
/// takes seconds and the load balancer moves the next try to the peer), never
/// longer than two seconds, for up to [`RECONNECT_BUDGET`] of failures in a row.
#[derive(Debug, Clone)]
pub struct Backoff {
    failing_since: Option<Instant>,
    attempt: u32,
    budget: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::with_budget(RECONNECT_BUDGET)
    }
}

impl Backoff {
    /// Give up after `budget` of failures in a row.
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            failing_since: None,
            attempt: 0,
            budget,
        }
    }

    /// The wait before the next try after a failure, or `None` once failures
    /// have lasted the whole budget.
    pub fn after_failure(&mut self, retry_after: Option<Duration>) -> Option<Duration> {
        let since = *self.failing_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.budget {
            return None;
        }
        let step =
            Duration::from_millis(100 * 2u64.pow(self.attempt.min(5))).min(Duration::from_secs(2));
        self.attempt += 1;
        // At least a moment even when told to retry at once, so a client never
        // spins against a control that keeps refusing.
        Some(
            retry_after
                .map(|r| r.clamp(Duration::from_millis(50), Duration::from_secs(5)))
                .unwrap_or(step),
        )
    }

    /// A request succeeded: the next failure starts a fresh budget.
    pub fn succeeded(&mut self) {
        self.failing_since = None;
        self.attempt = 0;
    }

    /// How long the current run of failures has lasted.
    pub fn failing_for(&self) -> Duration {
        self.failing_since.map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// Decodes one output stream to text without breaking a UTF-8 character
/// split across two pieces.
#[derive(Debug, Default)]
pub struct Utf8Text {
    partial: Vec<u8>,
}

impl Utf8Text {
    /// Text from `bytes`, holding back an incomplete trailing character.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(bytes);
        let mut text = String::new();
        let mut cursor = 0;
        while cursor < buf.len() {
            match std::str::from_utf8(&buf[cursor..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    cursor = buf.len();
                }
                Err(e) => {
                    let valid_end = cursor + e.valid_up_to();
                    text.push_str(std::str::from_utf8(&buf[cursor..valid_end]).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            text.push('\u{fffd}');
                            cursor = valid_end + len;
                        }
                        None => {
                            self.partial = buf[valid_end..].to_vec();
                            cursor = buf.len();
                        }
                    }
                }
            }
        }
        text
    }

    /// Whatever is held back, at the end of the stream.
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(output: &[(&str, u64, &[u8])], next: u64, done: bool) -> Vec<u8> {
        let output: Vec<_> = output
            .iter()
            .map(|(s, o, d)| {
                serde_json::json!({
                    "stream": s,
                    "offset": o,
                    "data": base64::engine::general_purpose::STANDARD.encode(d),
                })
            })
            .collect();
        let mut v = serde_json::json!({"output": output, "next": next, "done": done});
        if done {
            v["exitCode"] = 3.into();
        }
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn each_byte_is_delivered_once_whatever_is_repeated() {
        let mut r = Reader::new("m", "e");
        let a = r
            .absorb(&answer(&[("stdout", 0, b"hello ")], 6, false))
            .unwrap();
        assert_eq!(a.pieces, vec![Piece::Stdout(b"hello ".to_vec())]);
        // The same answer again (a retried read whose first answer did land).
        assert!(r
            .absorb(&answer(&[("stdout", 0, b"hello ")], 6, false))
            .unwrap()
            .pieces
            .is_empty());
        // An answer overlapping what was seen.
        let a = r
            .absorb(&answer(
                &[("stdout", 3, b"lo wor"), ("stderr", 9, b"!")],
                10,
                false,
            ))
            .unwrap();
        assert_eq!(
            a.pieces,
            vec![Piece::Stdout(b"wor".to_vec()), Piece::Stderr(b"!".to_vec())]
        );
        assert_eq!(r.next(), 10);
        assert!(r.read_path().ends_with("?after=10&wait=20"));
        let a = r.absorb(&answer(&[], 10, true)).unwrap();
        assert_eq!(a.done.unwrap().exit_code, Some(3));
    }

    #[test]
    fn the_end_is_not_reported_before_all_output_was_read() {
        let mut r = Reader::new("m", "e");
        // `done` refers to a `next` this reader has not reached (a hole).
        let a = r.absorb(&answer(&[("stdout", 2, b"xy")], 4, true)).unwrap();
        assert!(a.pieces.is_empty() && a.done.is_none());
        assert_eq!(r.next(), 0);
    }

    #[test]
    fn a_loss_is_reported_and_skipped() {
        let mut r = Reader::new("m", "e");
        let mut v: serde_json::Value =
            serde_json::from_slice(&answer(&[("stdout", 100, b"tail")], 104, false)).unwrap();
        v["lost"] = serde_json::json!({"from": 0, "to": 100});
        let a = r.absorb(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(a.lost, Some((0, 100)));
        assert_eq!(a.pieces, vec![Piece::Stdout(b"tail".to_vec())]);
    }

    #[test]
    fn statuses_are_classified() {
        assert_eq!(
            verdict(503, Some("control_draining"), false),
            Verdict::Retry
        );
        assert_eq!(
            verdict(503, Some("node_unreachable"), false),
            Verdict::Retry
        );
        assert_eq!(verdict(503, Some("control_draining"), true), Verdict::Retry);
        assert_eq!(verdict(503, None, true), Verdict::Retry);
        assert_eq!(verdict(503, Some("unavailable"), true), Verdict::Fail);
        assert_eq!(
            verdict(503, Some("machine_interrupted"), false),
            Verdict::Fail
        );
        assert_eq!(verdict(502, None, true), Verdict::Retry);
        assert_eq!(verdict(404, None, true), Verdict::Unsupported);
        assert_eq!(verdict(501, None, true), Verdict::Unsupported);
        assert_eq!(verdict(404, Some("exec_not_found"), false), Verdict::Fail);
        assert_eq!(verdict(409, None, true), Verdict::Fail);
        assert_eq!(problem_code(br#"{"code":"x"}"#).as_deref(), Some("x"));
        assert_eq!(problem_code(b"nope"), None);
    }

    #[test]
    fn backoff_grows_to_a_cap_and_gives_up_after_its_budget() {
        let mut b = Backoff::with_budget(Duration::from_millis(50));
        assert_eq!(b.after_failure(None), Some(Duration::from_millis(100)));
        assert_eq!(b.after_failure(None), Some(Duration::from_millis(200)));
        assert_eq!(
            b.after_failure(Some(Duration::from_secs(0))),
            Some(Duration::from_millis(50))
        );
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(b.after_failure(None), None);
        b.succeeded();
        assert!(b.after_failure(None).is_some());
    }

    #[test]
    fn ids_are_unique_and_valid() {
        let a = new_exec_id();
        let b = new_exec_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn split_characters_survive_decoding() {
        let mut t = Utf8Text::default();
        let s = "héllo ✓".as_bytes();
        let mut out = String::new();
        for b in s {
            out.push_str(&t.push(std::slice::from_ref(b)));
        }
        out.push_str(&t.finish());
        assert_eq!(out, "héllo ✓");
    }

    #[test]
    fn a_start_body_names_the_exec_and_waits() {
        let body = start_body(
            serde_json::json!({"command": ["true"], "stream": true, "background": false}),
            "abc",
        );
        assert_eq!(body["execId"], "abc");
        assert_eq!(body["waitSeconds"], READ_WAIT_SECS);
        assert!(body.get("stream").is_none() && body.get("background").is_none());
    }
}
