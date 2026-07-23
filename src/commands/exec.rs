//! smol machine exec — execute a command in a running machine.

use super::common;
use clap::Args;
use smolvm::agent::{AgentManager, ExecEvent, RunConfig};
use smolvm::db::SmolvmDb;
use std::time::Duration;

/// Windows stand-in for a SIGWINCH stream: `recv()` never completes, so the
/// terminal-resize select arm stays inert (Windows has no resize signal).
#[cfg(not(unix))]
struct WinchStub;
#[cfg(not(unix))]
impl WinchStub {
    async fn recv(&mut self) -> Option<()> {
        std::future::pending::<Option<()>>().await
    }
}

#[derive(Args, Debug)]
pub struct ExecCmd {
    /// Machine name (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Command to execute
    ///
    /// `last = true` requires the `--` separator so an old-style positional
    /// machine name (`smol machine exec myvm -- cmd`) fails loudly instead of
    /// being captured as the command and silently targeting "default" — the
    /// machine name is the `-n/--name` flag. Matches the engine CLI convention.
    #[arg(last = true, required = true, value_name = "COMMAND")]
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

    /// Force a cloud machine (by name or ID). Usually unnecessary — a machine's
    /// location is resolved automatically; equivalent to a `cloud/` prefix.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl ExecCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        // A machine's location is an attribute, not a command path: resolve it
        // from the reference (+ optional --local/--cloud override), then route.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::locate(self.name.as_deref(), target)?;
        // Normalize `name` to the bare handle so the downstream paths (which
        // still key on a plain name/id) see no `local/`/`cloud/` prefix.
        self.name = Some(handle);

        match location {
            Location::Cloud => {
                // An interactive/TTY session needs the PTY WebSocket; `--stream`
                // uses the SSE exec endpoint (real-time, no output cap); a plain
                // command uses the buffered HTTP exec.
                if self.interactive || self.tty {
                    self.run_cloud_interactive()
                } else if self.stream {
                    self.run_cloud_streaming()
                } else {
                    self.run_cloud()
                }
            }
            Location::Local => self.run_local(),
        }
    }

    fn run_local(self) -> anyhow::Result<()> {
        let name = super::common::resolve_name(self.name.clone());
        let command = strip_separator(&self.command);
        if command.is_empty() {
            anyhow::bail!(
                "no command specified.\n\
                 Use: smol machine exec --name <NAME> -- <command>"
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
        env.extend(common::resolve_cli_secrets(
            &self.secret_env,
            &self.secret_file,
        )?);

        // Check if this machine has an image — exec inside image rootfs if so.
        // Computed before the streaming branch so streamed execs on an image
        // machine also run in the persistent container overlay (see below).
        let record_image = record.as_ref().and_then(|r| r.image.clone());

        // Bind each of the machine's mounts into the exec's container:
        // (tag, guest_target, read_only), tag `smolvm{i}` in the VM's virtiofs
        // device order — the same form `smol run` and the engine's exec use.
        // Without this, an image machine whose workload command exits (alpine,
        // busybox, any finished CMD) has no live container, so this exec
        // re-establishes the keep-alive from an empty mount set and the
        // `/storage/workspace` fallback shadows the user's `-v host:/workspace`
        // — the mount silently vanishes inside exec sessions.
        let mount_bindings: Vec<(String, String, bool)> = record
            .as_ref()
            .map(|r| {
                r.host_mounts()
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        (
                            smolvm::data::storage::HostMount::mount_tag(i),
                            m.target.to_string_lossy().into_owned(),
                            m.read_only,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Streaming mode
        if self.stream {
            let mut exit_code = 0;
            let mut on_event = |event: ExecEvent| match event {
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
            };
            if let Some(ref image) = record_image {
                // Image machine: stream INSIDE the machine's persistent container
                // overlay so installs/writes survive across execs and restarts,
                // matching the non-streaming image path. Without this the stream
                // path runs in the bare agent rootfs and its changes are lost.
                let config = RunConfig::new(image, command.clone())
                    .with_env(env)
                    .with_workdir(self.workdir.clone())
                    .with_timeout(timeout)
                    .with_mounts(mount_bindings.clone())
                    .with_persistent_overlay(Some(name.clone()));
                client.run_streaming_with(config, on_event)?;
            } else {
                // Bare VM: stream directly against the guest.
                for event in
                    client.vm_exec_streaming(command.clone(), env, self.workdir.clone(), timeout)?
                {
                    on_event(event);
                }
            }
            manager.detach();
            std::process::exit(exit_code);
        }

        if let Some(ref image) = record_image {
            if self.interactive || self.tty {
                let config = RunConfig::new(image, command.clone())
                    .with_env(env)
                    .with_workdir(self.workdir.clone())
                    .with_timeout(timeout)
                    .with_tty(self.tty)
                    .with_mounts(mount_bindings.clone())
                    .with_persistent_overlay(Some(name.clone()));
                let exit_code = client.run_interactive(config)?;
                manager.detach();
                std::process::exit(exit_code);
            }

            let config = RunConfig::new(image, command.clone())
                .with_env(env)
                .with_workdir(self.workdir.clone())
                .with_timeout(timeout)
                .with_mounts(mount_bindings)
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
                "no command specified.\nUse: smol machine exec --cloud --name <NAME> -- <command>"
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
            anyhow::bail!("machine name or ID required for --cloud.\nUse: smol machine exec --cloud --name <NAME> -- <command>");
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

            let resp = super::cloud::check_response(resp, "exec").await?;

            let result: serde_json::Value = resp.json().await?;
            let exit_code = exec_exit_code(&result);

            // Prefer the byte-exact base64 output (binary-safe, untruncated);
            // fall back to the lossy UTF-8 text when talking to an older
            // control/node that only sends `stdout`/`stderr`.
            use base64::Engine as _;
            use std::io::Write as _;
            let decode = |b64_key: &str, text_key: &str| -> Vec<u8> {
                result
                    .get(b64_key)
                    .and_then(|v| v.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                    .unwrap_or_else(|| {
                        result
                            .get(text_key)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .as_bytes()
                            .to_vec()
                    })
            };
            let stdout_bytes = decode("stdoutB64", "stdout");
            let stderr_bytes = decode("stderrB64", "stderr");

            if !stdout_bytes.is_empty() {
                let mut out = std::io::stdout();
                let _ = out.write_all(&stdout_bytes);
                let _ = out.flush();
            }
            if !stderr_bytes.is_empty() {
                let mut err = std::io::stderr();
                let _ = err.write_all(&stderr_bytes);
                let _ = err.flush();
            }
            std::process::exit(exit_code);
        })
    }

    /// Stream a cloud command's output in real time over the SSE exec endpoint
    /// (`POST /v1/machines/{id}/exec/stream`). Unlike the buffered `run_cloud`,
    /// output is written as it arrives and is not capped — the right path for
    /// large or long-running output (`--stream`). The server frames output as
    /// `event: stdout|stderr|error|exit` + `data:` lines (SSE); the `exit`
    /// event's data is JSON `{ "exitCode": N }`.
    fn run_cloud_streaming(self) -> anyhow::Result<()> {
        let command = strip_separator(&self.command);
        if command.is_empty() {
            anyhow::bail!(
                "no command specified.\nUse: smol machine exec --cloud --stream --name <NAME> -- <command>"
            );
        }
        let env: std::collections::HashMap<String, String> =
            smolvm::util::parse_env_list(&self.env)
                .into_iter()
                .collect();
        let workdir = self.workdir.clone();
        let timeout = self.timeout;
        let name = self.name.clone();
        if name.is_none() {
            anyhow::bail!("machine name or ID required for --cloud.\nUse: smol machine exec --cloud --stream --name <NAME> -- <command>");
        }

        super::cloud::run_cloud_command(name, |http, endpoint, id| async move {
            use futures_util::StreamExt;
            use std::io::Write as _;

            let body = serde_json::json!({
                "command": command,
                "env": env,
                "cwd": workdir,
                "timeoutSeconds": timeout.unwrap_or(600),
            });
            let resp = http
                .post(format!("{}/v1/machines/{}/exec/stream", endpoint, id))
                .json(&body)
                .timeout(std::time::Duration::from_secs(timeout.unwrap_or(600) + 10))
                .send()
                .await?;
            let resp = super::cloud::check_response(resp, "exec stream").await?;

            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut event = String::new();
            let mut data_lines: Vec<String> = Vec::new();
            let mut exit_code: i32 = 0;
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();

            while let Some(chunk) = stream.next().await {
                buf.extend_from_slice(&chunk?);
                // Process complete lines; an empty line terminates one SSE frame.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=nl).collect();
                    let mut line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    if line.is_empty() {
                        let payload = data_lines.join("\n");
                        match event.as_str() {
                            "stdout" => {
                                let _ = out.write_all(payload.as_bytes());
                                let _ = out.flush();
                            }
                            "stderr" => {
                                let _ = err.write_all(payload.as_bytes());
                                let _ = err.flush();
                            }
                            "error" => {
                                eprintln!("error: {payload}");
                                exit_code = 1;
                            }
                            "exit" => {
                                exit_code = serde_json::from_str::<serde_json::Value>(&payload)
                                    .ok()
                                    .and_then(|v| v.get("exitCode").and_then(|c| c.as_i64()))
                                    .unwrap_or(0) as i32;
                            }
                            _ => {}
                        }
                        event.clear();
                        data_lines.clear();
                    } else if let Some(rest) = line.strip_prefix("event:") {
                        event = rest.trim().to_string();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    }
                }
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
                "machine name or ID required for --cloud.\nUse: smol machine shell --cloud --name <NAME>"
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
            .ok_or_else(|| anyhow::anyhow!("not logged in to the cloud; run `smol auth login`"))?;

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
            // Terminal-resize (SIGWINCH) forwarding is Unix-only; on Windows use a
            // never-ready stub so the resize select arm below simply never fires.
            #[cfg(unix)]
            let mut winch =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
            #[cfg(not(unix))]
            let mut winch = WinchStub;
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

/// Exit code to propagate from a cloud exec response.
///
/// `check_response` has already confirmed the exec API call itself succeeded, so
/// the JSON is a completed exec result. Use its `exitCode` when present; when it
/// is ABSENT, default to 0 (success) — the command completed and the server
/// reported no non-zero exit. Defaulting to 1 (the old behavior) wrongly failed
/// a succeeding command and broke `$?` for scripting whenever the field was
/// omitted.
fn exec_exit_code(result: &serde_json::Value) -> i32 {
    result.get("exitCode").and_then(|v| v.as_i64()).unwrap_or(0) as i32
}

#[cfg(test)]
mod exec_exit_code_tests {
    use super::exec_exit_code;
    use serde_json::json;

    #[test]
    fn present_exit_code_is_used() {
        assert_eq!(exec_exit_code(&json!({"exitCode": 0})), 0);
        assert_eq!(exec_exit_code(&json!({"exitCode": 3})), 3);
        assert_eq!(exec_exit_code(&json!({"exitCode": 137})), 137);
    }

    #[test]
    fn missing_exit_code_defaults_to_success_not_failure() {
        // A completed exec with no exitCode field must not report failure — the
        // GAP3 fix (was unwrap_or(1), breaking $? on a succeeding command).
        assert_eq!(exec_exit_code(&json!({"stdout": "ok\n"})), 0);
        assert_eq!(exec_exit_code(&json!({})), 0);
    }
}
