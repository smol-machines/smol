//! `smol agent`: run an agent harness (Claude Code, or any program) in its own
//! machine as a session of turns, with a checkpoint per turn so the session can
//! be rewound or branched. Built on the SDK's `smolmachines::agent`.

use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use smolmachines::agent::{AgentEvent, Harness, Session, SessionOptions};
use smolmachines::ConnectOptions;

#[derive(Args, Debug)]
pub struct AgentCmd {
    #[command(subcommand)]
    command: AgentSubcommand,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HarnessKind {
    /// Anthropic's Claude Code (needs ANTHROPIC_API_KEY)
    ClaudeCode,
    /// OpenAI's Codex CLI (needs OPENAI_API_KEY)
    Codex,
    /// OpenCode with any provider (`--model provider/model`)
    #[value(name = "opencode")]
    OpenCode,
    /// Any program: the prompt is appended as its last argument
    Command,
}

#[derive(Subcommand, Debug)]
enum AgentSubcommand {
    /// Start a session: create its machine and install the harness
    Start {
        /// Session name
        name: String,
        /// The agent program to run
        #[arg(long, value_enum, default_value = "claude-code")]
        harness: HarnessKind,
        /// Image for the `command` harness
        #[arg(long, default_value = "alpine:3.20")]
        image: String,
        /// Program for the `command` harness (the prompt is appended), e.g. "sh -c"
        #[arg(long = "program", default_value = "sh -c")]
        program: String,
        /// Model: optional for codex; `provider/model` for opencode, e.g.
        /// anthropic/claude-sonnet-4-5 or openai/gpt-5
        #[arg(long)]
        model: Option<String>,
        /// Run the session on smol cloud instead of the local engine
        #[arg(long)]
        cloud: bool,
        /// Another host the agent may reach (repeatable)
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
        /// Allow all outbound traffic instead of the host allow-list
        #[arg(long)]
        open_network: bool,
        /// Do not checkpoint after each turn (disables rewind and branch)
        #[arg(long)]
        no_checkpoints: bool,
        /// Pause the machine between turns so an idle agent holds no CPU
        #[arg(long)]
        pause_between_turns: bool,
        /// Install the harness from scratch instead of starting from a saved
        /// template of the same setup
        #[arg(long)]
        no_template: bool,
        /// vCPUs
        #[arg(long, default_value_t = 2)]
        cpus: u8,
        /// Memory in MiB
        #[arg(long, default_value_t = 2048)]
        memory: u32,
    },
    /// Send a prompt and stream what the agent does
    Send {
        /// Session name
        name: String,
        /// The prompt
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        prompt: Vec<String>,
        /// Print every event as a JSON line instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// List sessions
    Ls,
    /// Show a session's turns
    Log {
        /// Session name
        name: String,
    },
    /// Return a session to the state right after an earlier turn
    Rewind {
        /// Session name
        name: String,
        /// Turn number (from `smol agent log`)
        turn: usize,
    },
    /// Start a new session from the state right after a turn
    #[command(alias = "fork")]
    Branch {
        /// Session to branch
        name: String,
        /// Turn number to branch from
        turn: usize,
        /// Name of the new session
        new_name: String,
    },
    /// Pause a session's machine
    Pause {
        /// Session name
        name: String,
    },
    /// Resume a paused session's machine
    Resume {
        /// Session name
        name: String,
    },
    /// Delete a session, its machine and its checkpoints
    Rm {
        /// Session name
        name: String,
    },
    /// Serve sessions over HTTP; turns run in the service and survive clients
    Serve(super::agent_serve::AgentServeCmd),
}

impl AgentCmd {
    pub fn run(self) -> Result<()> {
        match self.command {
            AgentSubcommand::Start {
                name,
                harness,
                image,
                program,
                model,
                cloud,
                allow_hosts,
                open_network,
                no_checkpoints,
                pause_between_turns,
                no_template,
                cpus,
                memory,
            } => {
                let harness = match harness {
                    HarnessKind::ClaudeCode => Harness::ClaudeCode,
                    HarnessKind::Codex => Harness::Codex { model },
                    HarnessKind::OpenCode => {
                        let Some(model) = model else {
                            bail!("the opencode harness needs --model provider/model");
                        };
                        Harness::OpenCode { model }
                    }
                    HarnessKind::Command => {
                        let program: Vec<String> =
                            program.split_whitespace().map(str::to_string).collect();
                        if program.is_empty() {
                            bail!("--program must name a program");
                        }
                        Harness::Command { image, program }
                    }
                };
                let mut opts = SessionOptions::new(&name, harness);
                if cloud {
                    opts.connect = ConnectOptions::cloud();
                }
                opts.extra_hosts = allow_hosts;
                opts.open_network = open_network;
                opts.checkpoint_turns = !no_checkpoints;
                opts.pause_between_turns = pause_between_turns;
                opts.use_template = !no_template;
                opts.cpus = cpus;
                opts.memory_mib = memory;
                let session = Session::start(opts)?;
                let r = session.record();
                println!(
                    "Started agent session '{}' ({}) on {} machine {}",
                    r.name,
                    r.harness.name(),
                    r.target,
                    r.machine
                );
                if let Some(key) = r.harness.api_key_env() {
                    if r.key_outside_machine {
                        println!(
                            "{key} stays on this host: the machine holds a placeholder and the \
                             engine substitutes the key on requests to the provider."
                        );
                    } else {
                        println!(
                            "Each turn reads {key} from your environment; it is never stored."
                        );
                    }
                }
                Ok(())
            }
            AgentSubcommand::Send { name, prompt, json } => {
                let mut session = Session::open(&name)?;
                let prompt = prompt.join(" ");
                let mut last_text = None;
                let turn = session.send(&prompt, &mut |event| {
                    if json {
                        if let Ok(line) = serde_json::to_string(event) {
                            println!("{line}");
                        }
                        return;
                    }
                    match event {
                        // A failure the agent has not already written out.
                        AgentEvent::Finished {
                            result: Some(result),
                            is_error: true,
                            ..
                        } if last_text.as_ref() != Some(result) => println!("✗ {result}"),
                        AgentEvent::Text { text } => last_text = Some(text.clone()),
                        _ => {}
                    }
                    print_event(event);
                })?;
                if !json {
                    let cost = turn
                        .cost_usd
                        .map(|c| format!(", ${c:.4}"))
                        .unwrap_or_default();
                    let saved = if turn.checkpoint.is_some() {
                        ", checkpointed"
                    } else {
                        ""
                    };
                    let status = if turn.is_error { "failed" } else { "done" };
                    println!("── turn {} {status}{cost}{saved}", turn.index);
                }
                if turn.is_error {
                    std::process::exit(1);
                }
                Ok(())
            }
            AgentSubcommand::Ls => {
                let sessions = Session::list()?;
                if sessions.is_empty() {
                    println!("No agent sessions.");
                    return Ok(());
                }
                println!(
                    "{:<24} {:<12} {:<6} {:>5}  MACHINE",
                    "NAME", "HARNESS", "WHERE", "TURNS"
                );
                for s in sessions {
                    println!(
                        "{:<24} {:<12} {:<6} {:>5}  {}",
                        s.name,
                        s.harness.name(),
                        s.target,
                        s.turns.len(),
                        s.machine
                    );
                }
                Ok(())
            }
            AgentSubcommand::Log { name } => {
                let session = Session::open(&name)?;
                for t in &session.record().turns {
                    let result = t
                        .result
                        .as_deref()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("");
                    let mark = if t.is_error { "✗" } else { "✓" };
                    let cp = if t.checkpoint.is_some() { "⟲" } else { " " };
                    println!("{:>3} {mark}{cp} {}", t.index, one_line(&t.prompt, 60));
                    if !result.is_empty() {
                        println!("       → {}", one_line(result, 70));
                    }
                }
                Ok(())
            }
            AgentSubcommand::Rewind { name, turn } => {
                let mut session = Session::open(&name)?;
                session.rewind(turn)?;
                println!(
                    "Rewound '{name}' to turn {turn}; now on machine {}",
                    session.record().machine
                );
                Ok(())
            }
            AgentSubcommand::Branch {
                name,
                turn,
                new_name,
            } => {
                let session = Session::open(&name)?;
                let branch = session.branch(turn, &new_name)?;
                println!(
                    "Branched '{name}' at turn {turn} into '{new_name}' (machine {})",
                    branch.record().machine
                );
                Ok(())
            }
            AgentSubcommand::Pause { name } => {
                Session::open(&name)?.pause()?;
                println!("Paused '{name}'");
                Ok(())
            }
            AgentSubcommand::Resume { name } => {
                Session::open(&name)?.resume()?;
                println!("Resumed '{name}'");
                Ok(())
            }
            AgentSubcommand::Serve(cmd) => cmd.run(),
            AgentSubcommand::Rm { name } => {
                Session::open(&name)?.delete()?;
                println!("Deleted '{name}'");
                Ok(())
            }
        }
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::Text { text } => println!("{text}"),
        AgentEvent::ToolUse { name, input } => {
            let detail = input
                .get("command")
                .or_else(|| input.get("file_path"))
                .or_else(|| input.get("pattern"))
                .and_then(|v| v.as_str())
                .map(|s| one_line(s, 80))
                .unwrap_or_default();
            println!("▸ {name} {detail}");
        }
        AgentEvent::ToolResult { output, is_error } => {
            let first = output.lines().next().unwrap_or("");
            let mark = if *is_error { "✗" } else { " " };
            println!("  {mark} {}", one_line(first, 90));
        }
        AgentEvent::Finished { .. } | AgentEvent::Started { .. } | AgentEvent::Other(_) => {}
        AgentEvent::Stderr { line } => {
            if std::env::var_os("SMOL_AGENT_VERBOSE").is_some() {
                eprintln!("{line}");
            }
        }
    }
}
