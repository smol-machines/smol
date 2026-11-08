//! smol machine logs — stream machine output.

use super::common;
use clap::Args;
use smolvm::agent::ExecEvent;

#[derive(Args, Debug)]
pub struct LogsCmd {
    /// Machine name (default: "default")
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Follow log output
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Number of lines to show from the end
    #[arg(short = 'n', long, default_value = "100")]
    pub tail: u32,

    /// Show cloud machine events (by name or ID). Usually unnecessary — a
    /// machine's location is resolved automatically; equivalent to `cloud/`.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Show control-plane lifecycle events (created/started/stopped) instead of
    /// the machine's console output. Cloud only.
    #[arg(long)]
    pub events: bool,
}

impl LogsCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            self.name = Some(handle);
            return self.run_cloud();
        }
        let name = handle;

        let (manager, mut client) = common::ensure_connected(&name)?;
        manager.detach();

        // Follow mode: stream live output as it arrives so `-f` doesn't
        // block waiting for a process that never exits.
        if self.follow {
            let mut exit_code = 0;
            client.vm_exec_streaming_with(
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!("tail -n {} -f /var/log/messages 2>/dev/null || dmesg -w 2>/dev/null || echo 'No log source available'", self.tail),
                ],
                vec![],
                None,
                None,
                |event| match event {
                    ExecEvent::Stdout(data) => {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(&data);
                        let _ = std::io::stdout().flush();
                    }
                    ExecEvent::Stderr(data) => {
                        use std::io::Write;
                        let _ = std::io::stderr().write_all(&data);
                        let _ = std::io::stderr().flush();
                    }
                    ExecEvent::Exit(code) => exit_code = code,
                    ExecEvent::Error(msg) => {
                        eprintln!("error: {}", msg);
                        exit_code = 1;
                    }
                },
            )?;
            std::process::exit(exit_code);
        }

        // One-shot mode: read the tail once and print buffered output.
        let (exit_code, stdout, stderr) = client.vm_exec(
            vec![
                "sh".into(),
                "-c".into(),
                format!("tail -n {} /var/log/messages 2>/dev/null || dmesg 2>/dev/null || echo 'No log source available'", self.tail),
            ],
            vec![],
            None,
            None,
            None,
        )?;

        if !stdout.is_empty() {
            print!("{}", String::from_utf8_lossy(&stdout));
        }
        if !stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&stderr));
        }

        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        if self.events {
            return self.run_cloud_events();
        }
        let follow = self.follow;
        let tail = self.tail;
        super::cloud::run_cloud_command(self.name, |http, endpoint, id| async move {
            if follow {
                return follow_cloud_logs(&http, &endpoint, &id, tail).await;
            }
            // The machine's console (agent tracing + the workload's stdout/stderr)
            // — the same source the web console shows, and what `smol machine logs`
            // is expected to surface. `--events` switches to the control-plane
            // lifecycle feed instead.
            let resp = http
                .get(format!("{}/v1/machines/{}/logs", endpoint, id))
                .query(&[("follow", follow.to_string()), ("tail", tail.to_string())])
                .send()
                .await?;

            match resp.status().as_u16() {
                200 => {
                    use futures_util::StreamExt;
                    let mut stream = resp.bytes_stream();
                    let mut buf: Vec<u8> = Vec::new();
                    while let Some(chunk) = stream.next().await {
                        buf.extend_from_slice(&chunk?);
                        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=nl).collect();
                            print_console_line(&String::from_utf8_lossy(&line));
                        }
                    }
                    if !buf.is_empty() {
                        print_console_line(&String::from_utf8_lossy(&buf));
                    }
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                _ => {
                    super::cloud::check_response(resp, "stream logs").await?;
                }
            }
            Ok(())
        })
    }

    fn run_cloud_events(self) -> anyhow::Result<()> {
        super::cloud::run_cloud_command(self.name, |http, endpoint, id| async move {
            let resp = http
                .get(format!("{}/v1/machines/{}/events", endpoint, id))
                .send()
                .await?;

            match resp.status().as_u16() {
                200 => {
                    let events: Vec<serde_json::Value> = resp.json().await?;
                    if events.is_empty() {
                        eprintln!("No events.");
                        return Ok(());
                    }
                    for event in &events {
                        let ts = event
                            .get("createdAt")
                            .and_then(|v| v.as_str())
                            .unwrap_or("-");
                        let level = event
                            .get("level")
                            .and_then(|v| v.as_str())
                            .unwrap_or("info");
                        let msg = event.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        println!("{} [{}] {}", ts, level, msg);
                    }
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                _ => {
                    super::cloud::check_response(resp, "list events").await?;
                }
            }
            Ok(())
        })
    }
}

/// Follow a cloud machine's console, resuming after a dropped connection or a
/// control restart from the last line received: each event carries its log
/// offset as its id, and a reconnect asks for what follows it.
async fn follow_cloud_logs(
    http: &reqwest::Client,
    endpoint: &str,
    id: &str,
    tail: u32,
) -> anyhow::Result<()> {
    let mut last_id: Option<u64> = None;
    let mut backoff = smol_cloud::resumable::Backoff::default();
    loop {
        let mut query = vec![("follow", "true".to_string()), ("ids", "true".to_string())];
        match last_id {
            Some(after) => query.push(("after", after.to_string())),
            None => query.push(("tail", tail.to_string())),
        }
        // No overall deadline: a follow lasts as long as the user wants it.
        // A silent connection is noticed by the keep-alive watch instead.
        let sent = http
            .get(format!("{endpoint}/v1/machines/{id}/logs"))
            .query(&query)
            .timeout(std::time::Duration::from_secs(10 * 365 * 24 * 3600))
            .send()
            .await;
        let resp = match sent {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) if resp.status().as_u16() == 404 => {
                anyhow::bail!("machine '{}' not found", id)
            }
            Ok(resp) if !super::common::is_retryable_status(resp.status()) => {
                super::cloud::check_response(resp, "stream logs").await?;
                return Ok(());
            }
            _ => match backoff.after_failure(None) {
                Some(wait) => {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                None => anyhow::bail!("lost the control plane while following logs"),
            },
        };
        let seen_before = last_id;
        super::resumable::read_log_events(resp, &mut last_id, print_console_line).await?;
        if last_id.is_none() && seen_before.is_none() {
            // A control plane whose events carry no ids cannot resume a follow:
            // reading again would replay or skip lines. Say so instead.
            anyhow::bail!(
                "the log stream ended and this control plane cannot resume it; run the command again"
            );
        }
        // A stream that moved the follow on proves the way works; one that
        // ended at once (a control on its way down) counts as a failure, so
        // reconnects back off instead of spinning.
        if last_id != seen_before {
            backoff.succeeded();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        } else {
            match backoff.after_failure(None) {
                Some(wait) => tokio::time::sleep(wait).await,
                None => anyhow::bail!("lost the control plane while following logs"),
            }
        }
    }
}

/// Render one console log line. The node streams the guest's `agent-console.log`
/// as SSE `data: <json>` frames where the JSON is a tracing record; unwrap the
/// frame and print a compact `timestamp [LEVEL] message`, falling back to the
/// raw text for any line that isn't the expected shape (so nothing is dropped).
fn print_console_line(raw: &str) {
    let line = raw.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return;
    }
    let payload = line.strip_prefix("data: ").unwrap_or(line);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
        // `message` is either top-level or nested under tracing's `fields`.
        let msg = v.get("message").and_then(|m| m.as_str()).or_else(|| {
            v.get("fields")
                .and_then(|f| f.get("message"))
                .and_then(|m| m.as_str())
        });
        if let Some(msg) = msg {
            let ts = v.get("timestamp").and_then(|t| t.as_str());
            let level = v.get("level").and_then(|l| l.as_str()).unwrap_or("INFO");
            match ts {
                Some(ts) => println!("{ts} [{level}] {msg}"),
                None => println!("[{level}] {msg}"),
            }
            return;
        }
    }
    // Not a recognized JSON record — echo verbatim.
    println!("{payload}");
}
