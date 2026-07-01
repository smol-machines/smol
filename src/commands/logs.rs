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
                        let ts = event.get("createdAt").and_then(|v| v.as_str()).unwrap_or("-");
                        let level = event.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                        let msg = event.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        println!("{} [{}] {}", ts, level, msg);
                    }
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                status => {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("events failed ({}): {}", status, text);
                }
            }
            Ok(())
        })
    }
}
