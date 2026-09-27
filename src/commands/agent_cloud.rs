//! Hosted `smol agent --cloud` commands. Session state and turns live in the
//! control plane, so no local session record is read or written here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use smol_cloud::blocking::AgentStreamEvent;
use smol_cloud::types::{Agent, AgentTurn, CreateAgent, SendAgentTurn};
use smolmachines::agent::AgentEvent;
use smolmachines::cloud_agent::CloudAgentSession;
use smolmachines::ConnectOptions;

use super::agent::{one_line, print_event, AgentSubcommand, HarnessKind};

const SETUP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) fn run(command: AgentSubcommand) -> Result<()> {
    run_with(command, &ConnectOptions::cloud())
}

fn run_with(command: AgentSubcommand, connect: &ConnectOptions) -> Result<()> {
    match command {
        AgentSubcommand::Start {
            name,
            harness,
            image,
            program,
            model,
            credential,
            arch,
            allow_hosts,
            open_network,
            no_checkpoints,
            pause_between_turns,
            no_template,
            cpus,
            memory,
        } => {
            if pause_between_turns {
                bail!(
                    "hosted agents use explicit pause/resume; --pause-between-turns is local only"
                );
            }
            if no_template {
                bail!("--no-template is local only; hosted sessions install their harness on the cloud");
            }
            let (harness, image, program) = match harness {
                HarnessKind::ClaudeCode => ("claude-code", None, Vec::new()),
                HarnessKind::Codex => ("codex", None, Vec::new()),
                HarnessKind::OpenCode => {
                    if model.is_none() {
                        bail!("the opencode harness needs --model provider/model");
                    }
                    ("opencode", None, Vec::new())
                }
                HarnessKind::Command => {
                    let program: Vec<String> =
                        program.split_whitespace().map(str::to_string).collect();
                    if program.is_empty() {
                        bail!("--program must name a program");
                    }
                    if allow_hosts.is_empty() && !open_network {
                        bail!("the hosted command harness needs --allow-host or --open-network");
                    }
                    ("command", Some(image), program)
                }
            };
            let request = CreateAgent {
                name,
                harness: Some(harness.to_string()),
                model,
                image,
                program,
                allow_hosts,
                open_network,
                checkpoints: Some(!no_checkpoints),
                cpus: Some(cpus),
                memory_mb: Some(memory),
                arch,
                credential,
            };
            let session = CloudAgentSession::create(&request, connect)?;
            eprintln!(
                "Created hosted agent '{}'; waiting for setup…",
                session.name()
            );
            let info = wait_ready(&session)?;
            println!(
                "Hosted agent '{}' is ready ({}, machine {})",
                info.name,
                info.harness,
                info.machine_id.as_deref().unwrap_or("pending")
            );
            Ok(())
        }
        AgentSubcommand::Send {
            name,
            prompt,
            json,
            env_from,
            idempotency_key,
            timeout_seconds,
        } => {
            let session = CloudAgentSession::connect(&name, connect)?;
            wait_ready(&session)?;
            let mut env = HashMap::new();
            for key in env_from {
                let value = std::env::var(&key)
                    .with_context(|| format!("{key} is not set in this process's environment"))?;
                env.insert(key, value);
            }
            let request = SendAgentTurn {
                prompt: prompt.join(" "),
                env,
                timeout_seconds,
            };
            let turn = session.send(&request, idempotency_key.as_deref())?;
            follow_turn(&session, turn, json)
        }
        AgentSubcommand::Ls => {
            let mut after = None;
            let mut found = false;
            loop {
                let page = CloudAgentSession::list(connect, after.as_deref(), Some(100))?;
                for item in page.items {
                    if !found {
                        println!("{:<24} {:<13} {:<13} MACHINE", "NAME", "HARNESS", "STATUS");
                        found = true;
                    }
                    println!(
                        "{:<24} {:<13} {:<13} {}",
                        item.name,
                        item.harness,
                        item.status,
                        item.machine_id.as_deref().unwrap_or("—")
                    );
                }
                match page.next_cursor {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            if !found {
                println!("No hosted agent sessions.");
            }
            Ok(())
        }
        AgentSubcommand::Log { name } => {
            let info = CloudAgentSession::connect(&name, connect)?.info()?;
            println!("{} ({}, {})", info.name, info.harness, info.status);
            if let Some(error) = info.error {
                println!("  {error}");
            }
            for turn in info.turns {
                let mark = if turn.is_error { "✗" } else { "✓" };
                let cp = if turn.checkpointed { "⟲" } else { " " };
                println!(
                    "{:>3} {mark}{cp} {:<12} {}",
                    turn.index,
                    turn.status,
                    one_line(&turn.prompt, 60)
                );
                if let Some(result) = turn.result {
                    println!("       → {}", one_line(&result, 70));
                }
            }
            Ok(())
        }
        AgentSubcommand::Rewind { name, turn } => {
            let info = CloudAgentSession::connect(&name, connect)?.rewind(turn as u64)?;
            println!(
                "Rewound hosted agent '{}' to turn {turn} ({})",
                name, info.status
            );
            Ok(())
        }
        AgentSubcommand::Branch {
            name,
            turn,
            new_name,
        } => {
            let branch =
                CloudAgentSession::connect(&name, connect)?.branch(turn as u64, &new_name)?;
            let info = wait_ready(&branch)?;
            println!(
                "Branched hosted agent '{name}' at turn {turn} into '{}' ({})",
                info.name, info.status
            );
            Ok(())
        }
        AgentSubcommand::Pause { name } => {
            CloudAgentSession::connect(&name, connect)?.pause()?;
            println!("Paused hosted agent '{name}'");
            Ok(())
        }
        AgentSubcommand::Resume { name } => {
            CloudAgentSession::connect(&name, connect)?.resume()?;
            println!("Resumed hosted agent '{name}'");
            Ok(())
        }
        AgentSubcommand::Cancel { name, turn } => {
            CloudAgentSession::connect(&name, connect)?.cancel(turn)?;
            println!("Cancelled turn {turn} of hosted agent '{name}'");
            Ok(())
        }
        AgentSubcommand::Rm { name } => {
            CloudAgentSession::connect(&name, connect)?.delete()?;
            println!("Deleted hosted agent '{name}'");
            Ok(())
        }
        AgentSubcommand::Serve(_) => {
            bail!("smol agent serve is a self-hosted service; omit --cloud")
        }
    }
}

fn wait_ready(session: &CloudAgentSession) -> Result<Agent> {
    let started = Instant::now();
    loop {
        let info = session.info()?;
        match info.status.as_str() {
            "ready" => return Ok(info),
            "starting" | "rewinding" | "forking" | "resuming"
                if started.elapsed() < SETUP_TIMEOUT =>
            {
                std::thread::sleep(POLL_INTERVAL);
            }
            "failed" | "interrupted" => bail!(
                "hosted agent '{}' {}: {}",
                info.name,
                info.status,
                info.error
                    .as_deref()
                    .unwrap_or("check smol agent log --cloud")
            ),
            "starting" | "rewinding" | "forking" | "resuming" => {
                bail!(
                    "hosted agent '{}' is still {}; check smol agent log --cloud {}",
                    info.name,
                    info.status,
                    info.name
                )
            }
            _ => bail!(
                "hosted agent '{}' is {}; check smol agent log --cloud {}",
                info.name,
                info.status,
                info.name
            ),
        }
    }
}

fn follow_turn(session: &CloudAgentSession, turn: u64, json: bool) -> Result<()> {
    let mut after = None;
    let mut failures = 0;
    loop {
        let stream = match session.events(turn, after) {
            Ok(stream) => stream,
            Err(error) if failures < 3 => {
                failures += 1;
                eprintln!("Event stream disconnected ({error}); reconnecting…");
                std::thread::sleep(Duration::from_secs(failures));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let mut reconnect = false;
        for item in stream {
            match item {
                Ok(AgentStreamEvent::Event { id, data }) => {
                    if after.is_some_and(|last| id <= last) {
                        continue;
                    }
                    after = Some(id);
                    failures = 0;
                    if json {
                        println!("{data}");
                    } else if let Ok(event) = serde_json::from_value::<AgentEvent>(data.clone()) {
                        print_event(&event);
                    } else {
                        println!("{data}");
                    }
                }
                Ok(AgentStreamEvent::Done(outcome)) => return finish_turn(outcome, json),
                Err(error) if failures < 3 => {
                    failures += 1;
                    eprintln!("Event stream disconnected ({error}); reconnecting…");
                    std::thread::sleep(Duration::from_secs(failures));
                    reconnect = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !reconnect {
            bail!("event stream ended without a turn result");
        }
    }
}

fn finish_turn(turn: AgentTurn, json: bool) -> Result<()> {
    if !json {
        let cost = turn
            .cost_usd
            .map(|c| format!(", ${c:.4}"))
            .unwrap_or_default();
        let saved = if turn.checkpointed {
            ", checkpointed"
        } else {
            ""
        };
        println!("── turn {} {}{cost}{saved}", turn.index, turn.status);
    }
    if turn.is_error || turn.status != "done" {
        bail!(
            "turn {} {}: {}",
            turn.index,
            turn.status,
            turn.result.as_deref().unwrap_or("no result")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_one(listener: &TcpListener, expected: &str, status: &str, body: &str) -> String {
        let (mut socket, _) = listener.accept().expect("accept request");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).expect("read request");
            assert!(n > 0, "request closed before headers");
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        assert!(headers.starts_with(expected), "{headers}");
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|n| n.trim().parse().ok())
            })
            .unwrap_or(0);
        while request.len() - header_end < length {
            let n = socket.read(&mut buf).expect("read body");
            assert!(n > 0, "request closed before body");
            request.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8(request).expect("utf8 request");
        let content_type = if body.starts_with("event:") {
            "text/event-stream"
        } else {
            "application/json"
        };
        write!(
            socket,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
        text
    }

    #[test]
    fn cloud_cli_uses_hosted_sessions_and_follows_a_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock API");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let starting = r#"{"name":"fixer","harness":"claude-code","status":"starting","machineId":null,"runningTurn":null,"turns":[],"createdAt":"now"}"#;
            let ready = r#"{"name":"fixer","harness":"claude-code","status":"ready","machineId":"machine-1","runningTurn":null,"turns":[],"createdAt":"now"}"#;
            let create = serve_one(&listener, "POST /v1/agents ", "202 Accepted", starting);
            assert!(create.contains("\"credential\":\"anthropic\""), "{create}");
            assert!(!create.contains("\"cloud\":"), "{create}");
            for _ in 0..3 {
                serve_one(&listener, "GET /v1/agents/fixer ", "200 OK", ready);
            }
            let send = serve_one(
                &listener,
                "POST /v1/agents/fixer/turns ",
                "202 Accepted",
                r#"{"turn":0}"#,
            );
            assert!(send.contains("idempotency-key: turn-0"), "{send}");
            assert!(
                send.contains("\"prompt\":\"make the tests pass\""),
                "{send}"
            );
            let done = r#"{"index":0,"prompt":"make the tests pass","status":"done","result":"done","isError":false,"costUsd":null,"checkpointed":true,"startedAt":"now","finishedAt":"now"}"#;
            let sse = format!(
                "event: event\nid: 0\ndata: {{\"type\":\"text\",\"text\":\"working\"}}\n\nevent: done\ndata: {done}\n\n"
            );
            serve_one(
                &listener,
                "GET /v1/agents/fixer/turns/0/events ",
                "200 OK",
                &sse,
            );
        });
        let connect = ConnectOptions::with_api_key("test-key").base_url(url);
        run_with(
            AgentSubcommand::Start {
                name: "fixer".into(),
                harness: HarnessKind::ClaudeCode,
                image: "alpine:3.20".into(),
                program: "sh -c".into(),
                model: None,
                credential: Some("anthropic".into()),
                arch: None,
                allow_hosts: Vec::new(),
                open_network: false,
                no_checkpoints: false,
                pause_between_turns: false,
                no_template: false,
                cpus: 2,
                memory: 2048,
            },
            &connect,
        )
        .expect("hosted start");
        run_with(
            AgentSubcommand::Send {
                name: "fixer".into(),
                prompt: vec!["make the tests pass".into()],
                json: false,
                env_from: Vec::new(),
                idempotency_key: Some("turn-0".into()),
                timeout_seconds: None,
            },
            &connect,
        )
        .expect("hosted send");
        server.join().expect("server thread");
    }
}
