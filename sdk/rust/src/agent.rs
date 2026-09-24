//! Managed agents: an agent harness running in its own machine, one turn at a time.
//!
//! A [`Session`] pairs a machine with a *harness* — the agent program, such as
//! Claude Code — and runs the harness once per turn in headless mode, streaming
//! what it does as [`AgentEvent`]s. The machine keeps its disk between turns, so
//! the agent keeps its files, its installed tools and its own conversation state.
//!
//! After each turn the session can checkpoint the machine. Because the harness
//! stores its conversation inside the machine, restoring a checkpoint rewinds the
//! agent's *memory and its world together*: [`Session::rewind`] returns to the
//! state after an earlier turn, and [`Session::fork`] starts an independent
//! session from one.
//!
//! The same code drives a local engine or smol cloud; the session only records
//! which. Session records are small JSON files under `~/.smol/agents` (or
//! `SMOL_AGENTS_DIR`).
//!
//! ```no_run
//! use smolmachines::agent::{Harness, Session, SessionOptions};
//!
//! # fn main() -> smolmachines::Result<()> {
//! let mut session = Session::start(SessionOptions::new("fixer", Harness::ClaudeCode))?;
//! let turn = session.send("fix the failing test in /workspace", &mut |event| {
//!     println!("{event:?}");
//! })?;
//! println!("{}", turn.result.unwrap_or_default());
//! session.rewind(0)?; // back to the state right after the first turn
//! # Ok(())
//! # }
//! ```
//!
//! The model API key is passed to each turn's process environment from the
//! caller's environment at the moment of the turn; it is never written into the
//! machine's configuration or the session record.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ConnectOptions, Error, ErrorKind, ExecEvent, ExecOptions, Machine, MachineState, Result, Target,
};

/// The agent program a session runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Harness {
    /// Anthropic's Claude Code, run headless (`claude -p`) with streaming JSON
    /// output. Each turn resumes the previous turn's conversation.
    ClaudeCode,
    /// Any program: the prompt is appended as its last argument and each line it
    /// prints becomes a text event. Useful for custom agents and for tests.
    Command {
        /// Image the machine runs.
        image: String,
        /// The program and its leading arguments.
        program: Vec<String>,
    },
}

impl Harness {
    /// Short name for display and for machine names.
    pub fn name(&self) -> &str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Command { .. } => "command",
        }
    }

    fn image(&self) -> &str {
        match self {
            Harness::ClaudeCode => "node:22-bookworm-slim",
            Harness::Command { image, .. } => image,
        }
    }

    /// The environment variable holding the model API key, if the harness uses one.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            Harness::ClaudeCode => Some("ANTHROPIC_API_KEY"),
            Harness::Command { .. } => None,
        }
    }

    /// Hosts the harness needs: its model provider and the package registry it
    /// installs from.
    fn allowed_hosts(&self) -> &'static [&'static str] {
        match self {
            Harness::ClaudeCode => &["api.anthropic.com", "registry.npmjs.org"],
            Harness::Command { .. } => &[],
        }
    }

    /// Environment every turn runs with.
    fn turn_env(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            // IS_SANDBOX lets Claude Code skip permission prompts as root: the
            // machine is the sandbox. The rest keeps it from calling home.
            Harness::ClaudeCode => &[
                ("IS_SANDBOX", "1"),
                ("DISABLE_TELEMETRY", "1"),
                ("DISABLE_ERROR_REPORTING", "1"),
                ("DISABLE_AUTOUPDATER", "1"),
            ],
            Harness::Command { .. } => &[],
        }
    }

    /// One-time setup inside a new machine.
    fn setup_script(&self) -> Option<&'static str> {
        match self {
            Harness::ClaudeCode => Some(
                "set -e; mkdir -p /workspace; \
                 command -v claude >/dev/null 2>&1 || \
                 npm install -g --no-fund --no-audit --loglevel=error @anthropic-ai/claude-code",
            ),
            Harness::Command { .. } => Some("mkdir -p /workspace"),
        }
    }

    /// The command for one turn. `resume` is the harness's own conversation id
    /// from the previous turn.
    fn turn_command(&self, prompt: &str, resume: Option<&str>) -> Vec<String> {
        match self {
            Harness::ClaudeCode => {
                let mut cmd: Vec<String> = [
                    "claude",
                    "-p",
                    prompt,
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--dangerously-skip-permissions",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                if let Some(id) = resume {
                    cmd.push("--resume".into());
                    cmd.push(id.into());
                }
                cmd
            }
            Harness::Command { program, .. } => {
                let mut cmd = program.clone();
                cmd.push(prompt.into());
                cmd
            }
        }
    }

    /// Parse one line of the harness's output into events.
    pub fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        match self {
            Harness::ClaudeCode => parse_claude_line(line),
            Harness::Command { .. } => vec![AgentEvent::Text(line.to_string())],
        }
    }
}

/// Something an agent did during a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The harness started or resumed its conversation `session_id`.
    Started {
        /// The harness's own conversation id.
        session_id: String,
    },
    /// Text the agent wrote.
    Text(String),
    /// The agent called a tool.
    ToolUse {
        /// Tool name.
        name: String,
        /// Tool input.
        input: Value,
    },
    /// A tool returned.
    ToolResult {
        /// The tool's output, as text.
        output: String,
        /// Whether the tool reported an error.
        is_error: bool,
    },
    /// The turn finished.
    Finished {
        /// The agent's final answer.
        result: Option<String>,
        /// Whether the harness reported an error.
        is_error: bool,
        /// What the turn cost at the model provider, when the harness reports it.
        cost_usd: Option<f64>,
    },
    /// A line of the harness's stderr.
    Stderr(String),
    /// Output the parser did not recognise, kept verbatim.
    Other(Value),
}

fn parse_claude_line(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return if line.trim().is_empty() {
            vec![]
        } else {
            vec![AgentEvent::Text(line.to_string())]
        };
    };
    let str_of = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            str_of("session_id")
                .map(|session_id| vec![AgentEvent::Started { session_id }])
                .unwrap_or_default()
        }
        Some("assistant") => content_blocks(&v)
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|t| AgentEvent::Text(t.to_string())),
                Some("tool_use") => Some(AgentEvent::ToolUse {
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                }),
                _ => None,
            })
            .collect(),
        Some("user") => content_blocks(&v)
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|b| AgentEvent::ToolResult {
                output: tool_output_text(b.get("content")),
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect(),
        Some("result") => vec![AgentEvent::Finished {
            result: str_of("result"),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        }],
        _ => vec![AgentEvent::Other(v)],
    }
}

fn content_blocks(v: &Value) -> impl Iterator<Item = &Value> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn tool_output_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// How to start a session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Session name, unique among this user's sessions.
    pub name: String,
    /// The agent program.
    pub harness: Harness,
    /// Local engine or smol cloud.
    pub connect: ConnectOptions,
    /// Hosts the agent may reach beyond the harness's own (provider + registry).
    pub extra_hosts: Vec<String>,
    /// Allow all outbound traffic instead of the host allow-list.
    pub open_network: bool,
    /// Checkpoint the machine after every turn, making rewind and fork possible.
    pub checkpoint_turns: bool,
    /// Pause the machine between turns so an idle agent holds no CPU.
    pub pause_between_turns: bool,
    /// vCPUs for the machine.
    pub cpus: u8,
    /// Memory for the machine, in MiB.
    pub memory_mib: u32,
}

impl SessionOptions {
    /// Defaults: local engine, provider-only network, a checkpoint per turn.
    pub fn new(name: impl Into<String>, harness: Harness) -> Self {
        Self {
            name: name.into(),
            harness,
            connect: ConnectOptions::local(),
            extra_hosts: Vec::new(),
            open_network: false,
            checkpoint_turns: true,
            pause_between_turns: false,
            cpus: 2,
            memory_mib: 2048,
        }
    }
}

/// One completed turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    /// Zero-based turn number.
    pub index: usize,
    /// What the agent was asked.
    pub prompt: String,
    /// The agent's final answer.
    pub result: Option<String>,
    /// Whether the turn ended in an error.
    pub is_error: bool,
    /// Model cost the harness reported.
    pub cost_usd: Option<f64>,
    /// The harness conversation id after this turn, resumed by the next one.
    pub harness_session: Option<String>,
    /// The machine as it was right after this turn.
    pub checkpoint: Option<CheckpointRef>,
}

/// Where a turn's checkpoint lives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointRef {
    /// A local `.smolcheckpoint` file.
    Local {
        /// The file.
        path: PathBuf,
    },
    /// A checkpoint stored by smol cloud.
    Cloud {
        /// Its id.
        id: String,
    },
}

/// A session's durable record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session name.
    pub name: String,
    /// The agent program.
    pub harness: Harness,
    /// `local` or `cloud`.
    pub target: String,
    /// The machine currently backing the session.
    pub machine: String,
    /// Generation counter, bumped each time a rewind moves to a new machine.
    pub generation: u32,
    /// Hosts beyond the harness's own.
    pub extra_hosts: Vec<String>,
    /// All outbound traffic allowed.
    pub open_network: bool,
    /// Checkpoint after every turn.
    pub checkpoint_turns: bool,
    /// Pause between turns.
    pub pause_between_turns: bool,
    /// Completed turns, oldest first.
    pub turns: Vec<TurnRecord>,
}

/// An agent session. See the [module docs](self).
#[derive(Debug)]
pub struct Session {
    record: SessionRecord,
    dir: PathBuf,
}

/// The directory session records live in.
pub fn sessions_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SMOL_AGENTS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .map_err(|_| Error::new(ErrorKind::Config, "HOME is not set; set SMOL_AGENTS_DIR"))?;
    Ok(Path::new(&home).join(".smol").join("agents"))
}

fn io_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Other, format!("{context}: {e}"))
}

fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Config,
            format!("session name '{name}' must be 1-40 lowercase letters, digits or '-'"),
        ))
    }
}

fn connect_for(target: &str) -> ConnectOptions {
    if target == "cloud" {
        ConnectOptions::cloud()
    } else {
        ConnectOptions::local()
    }
}

impl Session {
    /// Create the machine, install the harness, and record the session.
    pub fn start(options: SessionOptions) -> Result<Session> {
        valid_name(&options.name)?;
        let dir = sessions_dir()?;
        if dir.join(format!("{}.json", options.name)).exists() {
            return Err(Error::new(
                ErrorKind::Config,
                format!("a session named '{}' already exists", options.name),
            ));
        }
        let target = match options.connect.target() {
            Target::Cloud => "cloud",
            _ => "local",
        };
        let record = SessionRecord {
            name: options.name.clone(),
            harness: options.harness.clone(),
            target: target.to_string(),
            machine: format!("agent-{}", options.name),
            generation: 0,
            extra_hosts: options.extra_hosts.clone(),
            open_network: options.open_network,
            checkpoint_turns: options.checkpoint_turns,
            pause_between_turns: options.pause_between_turns,
            turns: Vec::new(),
        };
        let mut builder = Machine::builder(&record.machine)
            .image(options.harness.image())
            .cpus(options.cpus)
            .memory_mib(options.memory_mib)
            .network(true)
            .branchable(options.checkpoint_turns)
            .label("smol.agent", &options.name)
            .label("smol.harness", options.harness.name());
        if !options.open_network {
            for host in options.harness.allowed_hosts() {
                builder = builder.allow_host(*host);
            }
            for host in &options.extra_hosts {
                builder = builder.allow_host(host);
            }
        }
        let machine = builder.create_with(&options.connect)?;
        if !matches!(
            machine.state(),
            MachineState::Running | MachineState::Started
        ) {
            if options.checkpoint_turns {
                machine.start_branchable()?;
            } else {
                machine.start()?;
            }
        }
        if let Some(script) = options.harness.setup_script() {
            let out = machine.exec(["sh", "-c", script])?;
            if !out.success() {
                let _ = machine.delete();
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "setting up {} failed: {}",
                        options.harness.name(),
                        out.stderr_utf8()
                    ),
                ));
            }
        }
        let session = Session { record, dir };
        session.save()?;
        if session.record.pause_between_turns {
            let _ = machine.pause();
        }
        Ok(session)
    }

    /// Load a recorded session.
    pub fn open(name: &str) -> Result<Session> {
        valid_name(name)?;
        let dir = sessions_dir()?;
        let path = dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::new(ErrorKind::NotFound, format!("no session '{name}': {e}")))?;
        let record: SessionRecord =
            serde_json::from_str(&text).map_err(|e| io_err("parse session record", e))?;
        Ok(Session { record, dir })
    }

    /// Every recorded session.
    pub fn list() -> Result<Vec<SessionRecord>> {
        let dir = sessions_dir()?;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(record) = serde_json::from_str::<SessionRecord>(&text) {
                        out.push(record);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The session's record.
    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    /// The machine backing the session, as it is: a paused or stopped session
    /// is not woken by looking at it. [`send`](Self::send) wakes it.
    pub fn machine(&self) -> Result<Machine> {
        if self.record.target == "cloud" {
            Machine::connect_with(&self.record.machine, &connect_for("cloud"))
        } else {
            Machine::attach(&self.record.machine)
        }
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_err("create sessions dir", e))?;
        let path = self.dir.join(format!("{}.json", self.record.name));
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.record).map_err(|e| io_err("encode", e))?;
        std::fs::write(&tmp, text).map_err(|e| io_err("write session record", e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_err("save session record", e))
    }

    fn ensure_running(&self, machine: &Machine) -> Result<()> {
        match machine.state() {
            MachineState::Running | MachineState::Started => Ok(()),
            MachineState::Paused | MachineState::Pausing => machine.resume(),
            _ => {
                let started = if self.record.checkpoint_turns {
                    machine.start_branchable()
                } else {
                    machine.start()
                };
                // A paused machine can report itself stopped; the engine then
                // refuses a start because execution was saved. Resume it instead.
                match started {
                    Err(e) if e.to_string().contains("saved execution") => machine.resume(),
                    other => other,
                }
            }
        }
    }

    /// Run one turn: ask the agent `prompt`, streaming its events to `on_event`.
    pub fn send(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnRecord> {
        let machine = self.machine()?;
        self.ensure_running(&machine)?;
        let harness = self.record.harness.clone();
        let resume = self
            .record
            .turns
            .last()
            .and_then(|t| t.harness_session.clone());
        let mut options = ExecOptions::new().workdir("/workspace");
        for (k, v) in harness.turn_env() {
            options = options.env(*k, *v);
        }
        if let Some(key_env) = harness.api_key_env() {
            let key = std::env::var(key_env).map_err(|_| {
                Error::new(
                    ErrorKind::Config,
                    format!(
                        "{key_env} is not set; the {} harness needs it",
                        harness.name()
                    ),
                )
            })?;
            options = options.env(key_env, key);
        }
        let command = harness.turn_command(prompt, resume.as_deref());
        let stream = machine.exec_stream(command, options)?;

        let mut turn = TurnRecord {
            index: self.record.turns.len(),
            prompt: prompt.to_string(),
            result: None,
            is_error: false,
            cost_usd: None,
            harness_session: resume,
            checkpoint: None,
        };
        let mut texts = Vec::new();
        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        let mut exit_code = None;
        let mut handle = |event: AgentEvent, turn: &mut TurnRecord, texts: &mut Vec<String>| {
            match &event {
                AgentEvent::Started { session_id } => {
                    turn.harness_session = Some(session_id.clone())
                }
                AgentEvent::Text(t) => texts.push(t.clone()),
                AgentEvent::Finished {
                    result,
                    is_error,
                    cost_usd,
                } => {
                    turn.result = result.clone();
                    turn.is_error = *is_error;
                    turn.cost_usd = *cost_usd;
                }
                _ => {}
            }
            on_event(&event);
        };
        for event in stream {
            match event {
                ExecEvent::Stdout(bytes) => {
                    stdout_buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = stdout_buf.find('\n') {
                        let line: String = stdout_buf.drain(..=pos).collect();
                        for e in harness.parse_line(line.trim_end_matches(['\r', '\n'])) {
                            handle(e, &mut turn, &mut texts);
                        }
                    }
                }
                ExecEvent::Stderr(bytes) => {
                    stderr_buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = stderr_buf.find('\n') {
                        let line: String = stderr_buf.drain(..=pos).collect();
                        handle(
                            AgentEvent::Stderr(line.trim_end().to_string()),
                            &mut turn,
                            &mut texts,
                        );
                    }
                }
                ExecEvent::Exit(code) => exit_code = Some(code),
                ExecEvent::Error(e) => return Err(Error::new(ErrorKind::Other, e)),
            }
        }
        if !stdout_buf.trim().is_empty() {
            for e in harness.parse_line(stdout_buf.trim_end()) {
                handle(e, &mut turn, &mut texts);
            }
        }
        if turn.result.is_none() && !texts.is_empty() {
            turn.result = Some(texts.join("\n"));
        }
        if exit_code.is_some_and(|c| c != 0) {
            turn.is_error = true;
        }

        if self.record.checkpoint_turns {
            turn.checkpoint = Some(self.checkpoint(&machine, turn.index)?);
        }
        self.record.turns.push(turn.clone());
        self.save()?;
        if self.record.pause_between_turns {
            let _ = machine.pause();
        }
        Ok(turn)
    }

    fn checkpoint_path(&self, index: usize) -> PathBuf {
        self.dir.join(&self.record.name).join(format!(
            "g{}-turn-{index}.smolcheckpoint",
            self.record.generation
        ))
    }

    fn checkpoint(&self, machine: &Machine, index: usize) -> Result<CheckpointRef> {
        if self.record.target == "cloud" {
            let cp = machine.checkpoint(None)?;
            let id = cp
                .cloud()
                .map(|c| c.id.clone())
                .ok_or_else(|| Error::new(ErrorKind::Other, "cloud checkpoint returned no id"))?;
            Ok(CheckpointRef::Cloud { id })
        } else {
            let path = self.checkpoint_path(index);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_err("create checkpoint dir", e))?;
            }
            machine.checkpoint(Some(&path))?;
            Ok(CheckpointRef::Local { path })
        }
    }

    fn restore(&self, checkpoint: &CheckpointRef, machine_name: &str) -> Result<Machine> {
        let machine = match checkpoint {
            CheckpointRef::Local { path } => Machine::restore_checkpoint(machine_name, path)?,
            CheckpointRef::Cloud { id } => {
                Machine::restore_cloud_checkpoint(machine_name, id, &connect_for("cloud"))?
            }
        };
        if !machine.is_running() {
            machine.start()?;
        }
        Ok(machine)
    }

    fn turn_checkpoint(&self, turn: usize) -> Result<CheckpointRef> {
        let record = self.record.turns.get(turn).ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                format!("session '{}' has no turn {turn}", self.record.name),
            )
        })?;
        record.checkpoint.clone().ok_or_else(|| {
            Error::new(
                ErrorKind::NotSupported,
                format!("turn {turn} has no checkpoint; start the session with checkpoints on"),
            )
        })
    }

    /// Return the session to the state right after `turn`: the machine is
    /// restored from that turn's checkpoint and later turns are forgotten.
    pub fn rewind(&mut self, turn: usize) -> Result<()> {
        let checkpoint = self.turn_checkpoint(turn)?;
        let next_generation = self.record.generation + 1;
        let new_machine = format!("agent-{}-g{next_generation}", self.record.name);
        self.restore(&checkpoint, &new_machine)?;
        if let Ok(old) = self.machine() {
            let _ = old.delete();
        }
        self.record.machine = new_machine;
        self.record.generation = next_generation;
        self.record.turns.truncate(turn + 1);
        self.save()
    }

    /// Start a new, independent session `name` from the state right after `turn`.
    pub fn fork(&self, turn: usize, name: &str) -> Result<Session> {
        valid_name(name)?;
        if self.dir.join(format!("{name}.json")).exists() {
            return Err(Error::new(
                ErrorKind::Config,
                format!("a session named '{name}' already exists"),
            ));
        }
        let checkpoint = self.turn_checkpoint(turn)?;
        let machine = format!("agent-{name}");
        self.restore(&checkpoint, &machine)?;
        let mut record = self.record.clone();
        record.name = name.to_string();
        record.machine = machine;
        record.generation = 0;
        record.turns.truncate(turn + 1);
        let session = Session {
            record,
            dir: self.dir.clone(),
        };
        session.save()?;
        Ok(session)
    }

    /// Pause the machine; the next [`send`](Self::send) resumes it.
    pub fn pause(&self) -> Result<()> {
        self.machine()?.pause()
    }

    /// Resume a paused machine.
    pub fn resume(&self) -> Result<()> {
        let machine = self.machine()?;
        self.ensure_running(&machine)
    }

    /// Delete the machine, the local checkpoints and the record.
    pub fn delete(self) -> Result<()> {
        if let Ok(machine) = self.machine() {
            machine.delete()?;
        }
        let _ = std::fs::remove_dir_all(self.dir.join(&self.record.name));
        std::fs::remove_file(self.dir.join(format!("{}.json", self.record.name)))
            .map_err(|e| io_err("remove session record", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stream_json_becomes_agent_events() {
        let h = Harness::ClaudeCode;
        assert_eq!(
            h.parse_line(r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#),
            vec![AgentEvent::Started {
                session_id: "abc".into()
            }]
        );
        assert_eq!(
            h.parse_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#
            ),
            vec![
                AgentEvent::Text("Looking.".into()),
                AgentEvent::ToolUse { name: "Bash".into(), input: serde_json::json!({"command": "ls"}) },
            ]
        );
        assert_eq!(
            h.parse_line(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"a.txt"}],"is_error":false}]}}"#
            ),
            vec![AgentEvent::ToolResult { output: "a.txt".into(), is_error: false }]
        );
        assert_eq!(
            h.parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","total_cost_usd":0.0123,"session_id":"abc"}"#),
            vec![AgentEvent::Finished { result: Some("Done.".into()), is_error: false, cost_usd: Some(0.0123) }]
        );
        assert!(h.parse_line("").is_empty());
        assert_eq!(
            h.parse_line("not json"),
            vec![AgentEvent::Text("not json".into())]
        );
    }

    #[test]
    fn claude_turns_resume_the_previous_conversation() {
        let h = Harness::ClaudeCode;
        let first = h.turn_command("hi", None);
        assert!(!first.contains(&"--resume".to_string()));
        let next = h.turn_command("again", Some("abc"));
        assert_eq!(&next[next.len() - 2..], ["--resume", "abc"]);
        assert!(next.contains(&"stream-json".to_string()));
    }

    #[test]
    fn command_harness_appends_the_prompt() {
        let h = Harness::Command {
            image: "alpine".into(),
            program: vec!["sh".into(), "-c".into()],
        };
        assert_eq!(h.turn_command("echo hi", None), ["sh", "-c", "echo hi"]);
        assert_eq!(h.parse_line("hi"), vec![AgentEvent::Text("hi".into())]);
    }

    #[test]
    fn session_names_are_validated() {
        assert!(valid_name("fix-bug-2").is_ok());
        for bad in ["", "Upper", "a b", "-lead", "x/../y"] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
    }
}
