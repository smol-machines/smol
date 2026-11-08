//! Resumable execs from the CLI: a command the machine's node owns, followed
//! across dropped connections and control restarts without losing or
//! repeating a byte. The protocol itself lives in `smol_cloud::resumable`;
//! this is its async driver over the CLI's HTTP client, plus the terminal
//! (attach WebSocket) and the reconnecting log follow.

use smol_cloud::resumable::{self, Backoff, Outcome, Piece, Reader, Verdict};
use std::collections::VecDeque;
use std::time::Duration;

/// How a request ended.
enum Sent {
    Answer(Vec<u8>),
    Unsupported,
}

/// Send one request until it has an answer worth acting on, retrying what a
/// control restart or a dropped connection causes (every request here is safe
/// to repeat: a start is idempotent on its exec id, a read on its offset).
async fn send_retrying(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<&serde_json::Value>,
    starting: bool,
) -> anyhow::Result<Sent> {
    let mut backoff = Backoff::default();
    loop {
        let mut request = http
            .request(method.clone(), url)
            .timeout(resumable::READ_TIMEOUT);
        if let Some(body) = body {
            request = request.json(body);
        }
        let answer = async {
            let resp = request.send().await?;
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let bytes = resp.bytes().await?;
            Ok::<_, reqwest::Error>((status, retry_after, bytes))
        }
        .await;
        let (status, retry_after, bytes) = match answer {
            Ok(answer) => answer,
            Err(e) => match backoff.after_failure(None) {
                Some(wait) => {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                None => anyhow::bail!(
                    "lost the control plane for {:?} ({e}); the command may still be running",
                    backoff.failing_for()
                ),
            },
        };
        if status.is_success() {
            return Ok(Sent::Answer(bytes.to_vec()));
        }
        let code = resumable::problem_code(&bytes);
        match resumable::verdict(status.as_u16(), code.as_deref(), starting) {
            Verdict::Unsupported => return Ok(Sent::Unsupported),
            Verdict::Retry => {
                if let Some(wait) = backoff.after_failure(retry_after) {
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            Verdict::Fail => {}
        }
        let sentence = smol_cloud::body_sentence(&String::from_utf8_lossy(&bytes));
        anyhow::bail!("{method} {url}: {status}: {sentence}");
    }
}

/// Run `command` (the exec body: `command`, `env`, `cwd`, `timeoutSeconds`,
/// `user`) as a resumable exec in machine `id`, handing each piece of output
/// to `on_piece` as it arrives, once. `None` when the control plane has no
/// resumable execs: the caller then uses the plain exec.
pub(super) async fn run(
    http: &reqwest::Client,
    endpoint: &str,
    id: &str,
    command: serde_json::Value,
    mut on_piece: impl FnMut(Piece),
) -> anyhow::Result<Option<Outcome>> {
    let exec_id = resumable::new_exec_id();
    let body = resumable::start_body(command, &exec_id);
    let start = format!("{endpoint}{}", resumable::start_path(id));
    let mut reader = Reader::new(id, &exec_id);
    let mut answer =
        match send_retrying(http, reqwest::Method::POST, &start, Some(&body), true).await? {
            Sent::Answer(answer) if resumable::is_exec_answer(&answer) => answer,
            Sent::Answer(_) | Sent::Unsupported => return Ok(None),
        };
    loop {
        let absorbed = reader.absorb(&answer).map_err(|e| anyhow::anyhow!(e))?;
        if let Some((from, to)) = absorbed.lost {
            eprintln!(
                "smol: {} bytes of output (offsets {from}..{to}) were dropped by the machine's node before this client read them",
                to - from
            );
        }
        for piece in absorbed.pieces {
            on_piece(piece);
        }
        if let Some(outcome) = absorbed.done {
            // Collected: let the node forget it now rather than at its TTL.
            let _ = http
                .delete(format!("{endpoint}{}", reader.delete_path()))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            return Ok(Some(outcome));
        }
        let read = format!("{endpoint}{}", reader.read_path());
        answer = match send_retrying(http, reqwest::Method::GET, &read, None, false).await? {
            Sent::Answer(answer) => answer,
            Sent::Unsupported => anyhow::bail!("the control plane stopped serving this exec"),
        };
    }
}

/// Start an interactive resumable exec (stdin kept open, a terminal when
/// `tty`), returning its id; `None` when the control plane has none.
pub(super) async fn start_interactive(
    http: &reqwest::Client,
    endpoint: &str,
    id: &str,
    mut command: serde_json::Value,
) -> anyhow::Result<Option<String>> {
    let exec_id = resumable::new_exec_id();
    if let Some(map) = command.as_object_mut() {
        map.insert("execId".into(), exec_id.clone().into());
        map.insert("interactive".into(), true.into());
        map.insert("waitSeconds".into(), 0.into());
    }
    let start = format!("{endpoint}{}", resumable::start_path(id));
    match send_retrying(http, reqwest::Method::POST, &start, Some(&command), true).await? {
        Sent::Answer(answer) if resumable::is_exec_answer(&answer) => Ok(Some(exec_id)),
        Sent::Answer(_) | Sent::Unsupported => Ok(None),
    }
}

/// The terminal side of an attach: stdin frames not yet acknowledged, and the
/// output offset reached.
#[derive(Default)]
pub(super) struct Terminal {
    /// Frames sent and not yet acknowledged, oldest first: (seq, bytes, eof).
    unacked: VecDeque<(u64, Vec<u8>, bool)>,
    last_seq: u64,
    /// The next output offset to read.
    pub next: u64,
}

/// What one frame from the node meant.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FromNode {
    /// Output to write (already trimmed of what was seen).
    Output(Vec<u8>),
    /// Frames to (re)send, after a hello.
    Resend(Vec<tokio_tungstenite::tungstenite::Message>),
    /// The command ended.
    Exit(Option<i32>, Option<String>),
    /// Output the node no longer held.
    Lost(u64, u64),
    Nothing,
}

impl Terminal {
    /// Stdin bytes as numbered frames of at most 64 KiB, remembered until
    /// acknowledged.
    pub fn stdin(&mut self, bytes: &[u8]) -> Vec<tokio_tungstenite::tungstenite::Message> {
        bytes
            .chunks(64 * 1024)
            .map(|chunk| {
                self.last_seq += 1;
                self.unacked
                    .push_back((self.last_seq, chunk.to_vec(), false));
                frame(self.last_seq, chunk, false)
            })
            .collect()
    }

    /// End of input, as a numbered frame.
    pub fn eof(&mut self) -> tokio_tungstenite::tungstenite::Message {
        self.last_seq += 1;
        self.unacked.push_back((self.last_seq, Vec::new(), true));
        frame(self.last_seq, &[], true)
    }

    fn acked(&mut self, applied: u64) {
        while self
            .unacked
            .front()
            .is_some_and(|(seq, _, _)| *seq <= applied)
        {
            self.unacked.pop_front();
        }
    }

    /// Take one frame from the node.
    pub fn take_frame(&mut self, msg: &tokio_tungstenite::tungstenite::Message) -> FromNode {
        use tokio_tungstenite::tungstenite::Message;
        match msg {
            Message::Binary(b) if b.len() >= 9 => {
                let offset = u64::from_be_bytes(b[1..9].try_into().unwrap_or([0; 8]));
                let data = &b[9..];
                let end = offset + data.len() as u64;
                if end <= self.next || offset > self.next {
                    return FromNode::Nothing;
                }
                let fresh = data[(self.next - offset) as usize..].to_vec();
                self.next = end;
                FromNode::Output(fresh)
            }
            Message::Text(t) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(t.as_str()) else {
                    return FromNode::Nothing;
                };
                match v["type"].as_str() {
                    Some("hello") => {
                        self.acked(v["stdinApplied"].as_u64().unwrap_or(0));
                        if let Some(next) = v["next"].as_u64() {
                            self.next = self.next.max(next);
                        }
                        FromNode::Resend(
                            self.unacked
                                .iter()
                                .map(|(seq, data, eof)| frame(*seq, data, *eof))
                                .collect(),
                        )
                    }
                    Some("stdin") => {
                        self.acked(v["applied"].as_u64().unwrap_or(0));
                        FromNode::Nothing
                    }
                    Some("lost") => {
                        let (from, to) = (
                            v["from"].as_u64().unwrap_or(0),
                            v["to"].as_u64().unwrap_or(0),
                        );
                        self.next = self.next.max(to);
                        FromNode::Lost(from, to)
                    }
                    Some("exit") => FromNode::Exit(
                        v["exitCode"].as_i64().map(|c| c as i32),
                        v["error"].as_str().map(str::to_string),
                    ),
                    _ => FromNode::Nothing,
                }
            }
            _ => FromNode::Nothing,
        }
    }

    /// The acknowledgement of output received so far.
    pub fn ack(&self) -> tokio_tungstenite::tungstenite::Message {
        tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"type": "ack", "offset": self.next})
                .to_string()
                .into(),
        )
    }
}

fn frame(seq: u64, data: &[u8], eof: bool) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message;
    if eof {
        return Message::Text(
            serde_json::json!({"type": "eof", "seq": seq})
                .to_string()
                .into(),
        );
    }
    let mut f = seq.to_be_bytes().to_vec();
    f.extend_from_slice(data);
    Message::Binary(f.into())
}

/// Read a log follow's SSE body, printing each line with `print` and keeping
/// the `id` of the last event, so a dropped stream resumes after it. Returns
/// whether the server asked to reconnect (or the stream just ended).
pub(super) async fn read_log_events(
    resp: reqwest::Response,
    last_id: &mut Option<u64>,
    mut print: impl FnMut(&str),
) -> anyhow::Result<bool> {
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut event_id: Option<u64> = None;
    let mut event_kind = String::new();
    let mut data: Vec<String> = Vec::new();
    loop {
        // Keep-alives come every 15 s; silence for longer is a dead connection.
        let chunk = match tokio::time::timeout(Duration::from_secs(45), stream.next()).await {
            Err(_) => return Ok(true),
            Ok(None) => return Ok(true),
            Ok(Some(Err(_))) => return Ok(true),
            Ok(Some(Ok(chunk))) => chunk,
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            if line.is_empty() {
                if event_kind == "reconnect" {
                    return Ok(true);
                }
                if !data.is_empty() || event_id.is_some() {
                    print(&data.join("\n"));
                }
                if event_id.is_some() {
                    *last_id = event_id;
                }
                event_id = None;
                event_kind.clear();
                data.clear();
            } else if let Some(rest) = line.strip_prefix("id:") {
                event_id = rest.trim().parse().ok();
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_kind = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Message;

    fn output(offset: u64, data: &[u8]) -> Message {
        let mut f = vec![1u8];
        f.extend_from_slice(&offset.to_be_bytes());
        f.extend_from_slice(data);
        Message::Binary(f.into())
    }

    #[test]
    fn a_reattached_terminal_resends_only_unapplied_stdin_and_skips_seen_output() {
        let mut t = Terminal::default();
        let sent = t.stdin(b"ls\n");
        assert_eq!(sent.len(), 1);
        t.stdin(b"pwd\n");
        assert_eq!(
            t.take_frame(&output(0, b"a.txt\n")),
            FromNode::Output(b"a.txt\n".to_vec())
        );
        // The connection drops; the new attach says frame 1 was applied.
        let hello = Message::Text(
            serde_json::json!({"type": "hello", "stdinApplied": 1, "next": 3})
                .to_string()
                .into(),
        );
        match t.take_frame(&hello) {
            FromNode::Resend(frames) => {
                assert_eq!(frames.len(), 1);
                match &frames[0] {
                    Message::Binary(b) => {
                        assert_eq!(u64::from_be_bytes(b[..8].try_into().unwrap()), 2);
                        assert_eq!(&b[8..], b"pwd\n");
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
        // Output resent from an older offset is trimmed to what is new.
        assert_eq!(
            t.take_frame(&output(2, b"txt\n/root\n")),
            FromNode::Output(b"/root\n".to_vec())
        );
        assert_eq!(t.next, 12);
        let exit = Message::Text(
            serde_json::json!({"type": "exit", "exitCode": 0})
                .to_string()
                .into(),
        );
        assert_eq!(t.take_frame(&exit), FromNode::Exit(Some(0), None));
    }
}
