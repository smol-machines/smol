//! smol exec — execute a command in a running machine.

use super::common;
use clap::Args;
use smolvm::agent::{AgentManager, ExecEvent, RunConfig};
use smolvm::db::SmolvmDb;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct ExecCmd {
    /// Machine name (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Command to execute
    #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,

    /// Keep stdin open
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long)]
    pub tty: bool,

    /// Stream output in real-time
    #[arg(long)]
    pub stream: bool,

    /// Set environment variable (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Set working directory
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Inject a secret from a host env var (GUEST_VAR=HOST_VAR), resolved on the
    /// host for this exec; never persisted
    #[arg(long = "secret-env", value_name = "GUEST_VAR=HOST_VAR")]
    pub secret_env: Vec<String>,

    /// Inject a secret from a host file (GUEST_VAR=/abs/path), resolved on the
    /// host for this exec; never persisted
    #[arg(long = "secret-file", value_name = "GUEST_VAR=PATH")]
    pub secret_file: Vec<String>,

    /// Timeout in seconds
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Execute in a cloud machine (by name or ID)
    #[arg(long)]
    pub cloud: bool,
}

impl ExecCmd {
    pub fn run(self) -> anyhow::Result<()> {
        if self.cloud {
            // An interactive/TTY session needs the PTY WebSocket; a plain command
            // uses the buffered HTTP exec.
            if self.interactive || self.tty {
                return self.run_cloud_interactive();
            }
            return self.run_cloud();
        }

        let name = super::common::resolve_name(self.name.clone());
        let command = strip_separator(&self.command);
        if command.is_empty() {
            anyhow::bail!(
                "no command specified.\n\
                 Use: smol exec --name <NAME> -- <command>"
            );
        }
        let timeout = self.timeout.map(Duration::from_secs);

        let (manager, mut client) = common::ensure_connected(&name)?;
        manager.detach();

        // Load the record once: its image (workload mode) and its persisted
        // secret_refs (injected into every exec, like the engine).
        let record = SmolvmDb::open()
            .ok()
            .and_then(|db| db.get_vm(&name).ok().flatten());

        let mut env = smolvm::util::parse_env_list(&self.env);
        // Machine's stored secrets first, then per-exec --secret-env/--secret-file.
        if let Some(ref r) = record {
            env.extend(common::resolve_record_secrets(&r.secret_refs)?);
        }
        env.extend(common::resolve_cli_secrets(&self.secret_env, &self.secret_file)?);

        // Streaming mode
        if self.stream {
            let events =
                client.vm_exec_streaming(command.clone(), env, self.workdir.clone(), timeout)?;
            let mut exit_code = 0;
            for event in events {
                match event {
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
                }
            }
            manager.detach();
            std::process::exit(exit_code);
        }

        // Check if this machine has an image — exec inside image rootfs if so
        let record_image = record.as_ref().and_then(|r| r.image.clone());

        if let Some(ref image) = record_image {
            if self.interactive || self.tty {
                let config = RunConfig::new(image, command.clone())
                    .with_env(env)
                    .with_workdir(self.workdir.clone())
                    .with_timeout(timeout)
                    .with_tty(self.tty)
                    // Run in the machine's persistent overlay so filesystem
                    // changes survive across execs (and join the running main
                    // container when started detached). Without this each exec
                    // gets a fresh overlay and writes to `/` vanish.
                    .with_persistent_overlay(Some(name.clone()));
                let exit_code = client.run_interactive(config)?;
                manager.detach();
                std::process::exit(exit_code);
            }

            let config = RunConfig::new(image, command.clone())
                .with_env(env)
                .with_workdir(self.workdir.clone())
                .with_timeout(timeout)
                .with_persistent_overlay(Some(name.clone()));
            let (exit_code, stdout, stderr) = client.run_non_interactive(config)?;
            print_and_exit(
                &manager,
                exit_code,
                &String::from_utf8_lossy(&stdout),
                &String::from_utf8_lossy(&stderr),
            );
        } else {
            // Bare VM mode
            if self.interactive || self.tty {
                let exit_code = client.vm_exec_interactive(
                    command.clone(),
                    env,
                    self.workdir.clone(),
                    timeout,
                    self.tty,
                )?;
                manager.detach();
                std::process::exit(exit_code);
            }

            let (exit_code, stdout, stderr) =
                client.vm_exec(command.clone(), env, self.workdir.clone(), timeout, None)?;
            print_and_exit(
                &manager,
                exit_code,
                &String::from_utf8_lossy(&stdout),
                &String::from_utf8_lossy(&stderr),
            );
        }
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        // Pre-validate before entering the cloud command helper
        let command = strip_separator(&self.command);
        if command.is_empty() {
            anyhow::bail!(
                "no command specified.\nUse: smol exec --cloud --name <NAME> -- <command>"
            );
        }

        let env: std::collections::HashMap<String, String> =
            smolvm::util::parse_env_list(&self.env)
                .into_iter()
                .collect();
        let workdir = self.workdir.clone();
        let timeout = self.timeout;

        // Override the default error message for exec
        let name = self.name.clone();
        if name.is_none() {
            anyhow::bail!("machine name or ID required for --cloud.\nUse: smol exec --cloud --name <NAME> -- <command>");
        }

        super::cloud::run_cloud_command(name, |http, endpoint, id| async move {
            let body = serde_json::json!({
                "command": command,
                "env": env,
                "cwd": workdir,
                "timeoutSeconds": timeout.unwrap_or(600),
            });

            let resp = http
                .post(format!("{}/v1/machines/{}/exec", endpoint, id))
                .json(&body)
                .timeout(std::time::Duration::from_secs(timeout.unwrap_or(600) + 10))
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("exec failed ({}): {}", status, text);
            }

            let result: serde_json::Value = resp.json().await?;
            let stdout = result.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = result.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let exit_code = result.get("exitCode").and_then(|v| v.as_i64()).unwrap_or(1) as i32;

            if !stdout.is_empty() {
                print!("{}", stdout);
            }
            if !stderr.is_empty() {
                eprint!("{}", stderr);
            }
            std::process::exit(exit_code);
        })
    }

    /// Interactive cloud exec/shell over the PTY WebSocket
    /// (`GET /v1/machines/{id}/exec/interactive`). Stdin is streamed as binary
    /// frames, stdout arrives as binary frames, and resize/exit are JSON text
    /// frames — matching the node's protocol.
    fn run_cloud_interactive(self) -> anyhow::Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use smolvm::agent::terminal;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_tungstenite::tungstenite::Message;

        let command = strip_separator(&self.command);
        if command.is_empty() {
            anyhow::bail!("no command specified");
        }
        let name = self.name.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "machine name or ID required for --cloud.\nUse: smol shell --cloud --name <NAME>"
            )
        })?;
        let cmd = command.join(" ");

        // Resolve credentials BEFORE the runtime (cloud_client may block_on a
        // token refresh; see run_cloud_command's note on the sync/async split).
        let (http, cloud_config) = super::cloud::cloud_client()?;
        let endpoint = cloud_config.endpoint()?.to_string();
        let token = cloud_config
            .api_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("not logged in to the cloud; run `smol login`"))?;

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async move {
            let id = super::cloud::resolve_machine_id(&http, &endpoint, &name).await?;
            let ws_base = endpoint
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1);
            let (cols, rows) = terminal::get_terminal_size().unwrap_or((80, 24));
            let url = format!(
                "{ws_base}/v1/machines/{id}/exec/interactive?cmd={}&cols={cols}&rows={rows}",
                pct_encode(&cmd),
            );

            // Send the bearer token in the `Authorization` header rather than an
            // `?access_token=` query param: the URL (with query) shows up in
            // server/proxy access logs, leaking the credential. The server
            // prefers the header and only falls back to the query param for
            // browser WebSocket clients, which (unlike this CLI) can't set
            // headers on `new WebSocket()`.
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
            let mut request = url
                .into_client_request()
                .map_err(|e| anyhow::anyhow!("build interactive exec request: {e}"))?;
            let auth = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("invalid authorization header: {e}"))?;
            request.headers_mut().insert(AUTHORIZATION, auth);

            let (ws, _resp) = tokio_tungstenite::connect_async(request)
                .await
                .map_err(|e| anyhow::anyhow!("connect interactive exec: {e}"))?;
            let (mut ws_tx, mut ws_rx) = ws.split();

            // Raw terminal so keystrokes go straight to the remote PTY. Skipped
            // when stdin isn't a TTY (e.g. piped) — the byte pump still works.
            let _raw = if terminal::stdin_is_tty() {
                terminal::RawModeGuard::new(0)
            } else {
                None
            };

            let mut stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();
            let mut buf = vec![0u8; 8192];
            let mut stdin_open = true;
            let mut winch =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
            let mut exit_code = 0i32;

            loop {
                tokio::select! {
                    r = stdin.read(&mut buf), if stdin_open => match r {
                        Ok(0) => stdin_open = false, // EOF: stop reading; keep the session up
                        Ok(n) => {
                            if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => stdin_open = false,
                    },
                    _ = winch.recv() => {
                        if let Some((c, r)) = terminal::get_terminal_size() {
                            let frame = format!("{{\"type\":\"resize\",\"cols\":{c},\"rows\":{r}}}");
                            let _ = ws_tx.send(Message::Text(frame.into())).await;
                        }
                    }
                    msg = ws_rx.next() => match msg {
                        Some(Ok(Message::Binary(b))) => {
                            stdout.write_all(&b).await?;
                            stdout.flush().await?;
                        }
                        Some(Ok(Message::Text(t))) => {
                            match serde_json::from_str::<serde_json::Value>(t.as_str()) {
                                Ok(v) if v["type"] == "exit" => {
                                    exit_code = v["code"].as_i64().unwrap_or(0) as i32;
                                    break;
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    stdout.write_all(t.as_bytes()).await?;
                                    stdout.flush().await?;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow::anyhow!("interactive exec stream: {e}")),
                    },
                }
            }
            drop(_raw); // restore the terminal before the hard exit
            std::process::exit(exit_code);
        })
    }
}

/// Percent-encode a query-string value, leaving RFC 3986 unreserved chars (and
/// `/`, harmless in a query) intact.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Strip a leading `--` separator from a command vec if present.
fn strip_separator(args: &[String]) -> Vec<String> {
    if args.first().map(|s| s.as_str()) == Some("--") {
        args[1..].to_vec()
    } else {
        args.to_vec()
    }
}

fn print_and_exit(manager: &AgentManager, exit_code: i32, stdout: &str, stderr: &str) -> ! {
    if !stdout.is_empty() {
        print!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprint!("{}", stderr);
    }
    manager.detach();
    std::process::exit(exit_code);
}
