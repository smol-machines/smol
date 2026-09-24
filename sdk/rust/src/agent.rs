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
    /// OpenAI's Codex CLI, run headless (`codex exec --json`). Each turn resumes
    /// the previous turn's thread.
    Codex {
        /// Model to use instead of Codex's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// OpenCode, run headless (`opencode run --format json`) with any provider it
    /// supports. Each turn continues the previous turn's session.
    #[serde(rename = "opencode")]
    OpenCode {
        /// `provider/model`, e.g. `anthropic/claude-sonnet-4-5`. The provider
        /// decides which API key the session needs.
        model: String,
    },
    /// Any program: the prompt is appended as its last argument and each line it
    /// prints becomes a text event. Useful for custom agents and for tests.
    Command {
        /// Image the machine runs.
        image: String,
        /// The program and its leading arguments.
        program: Vec<String>,
    },
}

/// A model provider an agent calls: the key it needs and where it sends it.
struct Provider {
    key_env: &'static str,
    host: &'static str,
}

const ANTHROPIC: Provider = Provider {
    key_env: "ANTHROPIC_API_KEY",
    host: "api.anthropic.com",
};

const OPENAI: Provider = Provider {
    key_env: "OPENAI_API_KEY",
    host: "api.openai.com",
};

/// The model provider Codex runs with.
const CODEX_PROVIDER: &str = r#"model_providers.smol={name="OpenAI",base_url="https://api.openai.com/v1",env_key="OPENAI_API_KEY",wire_api="responses",supports_websockets=false}"#;

/// Installs a harness from npm into a machine that has Node.
const NPM_INSTALL: &str = "npm install -g --no-fund --no-audit --loglevel=error";

impl Harness {
    /// Short name for display and for machine names.
    pub fn name(&self) -> &str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex { .. } => "codex",
            Harness::OpenCode { .. } => "opencode",
            Harness::Command { .. } => "command",
        }
    }

    fn image(&self) -> &str {
        match self {
            Harness::ClaudeCode | Harness::Codex { .. } | Harness::OpenCode { .. } => {
                "node:22-bookworm-slim"
            }
            Harness::Command { image, .. } => image,
        }
    }

    fn provider(&self) -> Option<Provider> {
        match self {
            Harness::ClaudeCode => Some(ANTHROPIC),
            Harness::Codex { .. } => Some(OPENAI),
            Harness::OpenCode { model } => match model.split_once('/').map(|(p, _)| p) {
                Some("anthropic") => Some(ANTHROPIC),
                Some("openai") => Some(OPENAI),
                _ => None,
            },
            Harness::Command { .. } => None,
        }
    }

    /// The environment variable holding the model API key, if the harness uses one.
    pub fn api_key_env(&self) -> Option<&'static str> {
        self.provider().map(|p| p.key_env)
    }

    /// The model provider the API key is sent to.
    fn provider_host(&self) -> Option<&'static str> {
        self.provider().map(|p| p.host)
    }

    /// Hosts the harness needs: its model provider and where it installs from.
    fn allowed_hosts(&self) -> Vec<&'static str> {
        let mut hosts: Vec<&'static str> = self.provider_host().into_iter().collect();
        match self {
            Harness::ClaudeCode | Harness::Codex { .. } => hosts.push("registry.npmjs.org"),
            // OpenCode reads its model catalogue from models.dev, and its own
            // `opencode/*` models are served by opencode.ai.
            Harness::OpenCode { .. } => {
                hosts.extend(["registry.npmjs.org", "models.dev", "opencode.ai"])
            }
            Harness::Command { .. } => {}
        }
        hosts
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
            Harness::OpenCode { .. } => &[("OPENCODE_DISABLE_AUTOUPDATE", "1")],
            Harness::Codex { .. } | Harness::Command { .. } => &[],
        }
    }

    /// One-time setup inside a new machine.
    fn setup_script(&self) -> Option<String> {
        let install = |bin: &str, package: &str| {
            format!(
                "set -e; mkdir -p /workspace; \
                 command -v {bin} >/dev/null 2>&1 || {NPM_INSTALL} {package}"
            )
        };
        match self {
            Harness::ClaudeCode => Some(install("claude", "@anthropic-ai/claude-code")),
            // Codex verifies TLS against the system roots, which the slim image
            // lacks; Node carries Mozilla's, so write those out.
            Harness::Codex { .. } => Some(format!(
                "{}; test -s /etc/ssl/certs/ca-certificates.crt || {{ mkdir -p /etc/ssl/certs; \
                 node -e 'process.stdout.write(require(\"tls\").rootCertificates.join(\"\\n\") + \"\\n\")' \
                 > /etc/ssl/certs/ca-certificates.crt; }}",
                install("codex", "@openai/codex")
            )),
            Harness::OpenCode { .. } => Some(install("opencode", "opencode-ai")),
            Harness::Command { .. } => Some("mkdir -p /workspace".into()),
        }
    }

    /// The command for one turn. `resume` is the harness's own conversation id
    /// from the previous turn.
    fn turn_command(&self, prompt: &str, resume: Option<&str>) -> Vec<String> {
        let mut cmd: Vec<String> = Vec::new();
        let mut push = |args: &[&str]| cmd.extend(args.iter().map(|s| s.to_string()));
        match self {
            Harness::ClaudeCode => {
                push(&["claude", "-p", prompt, "--output-format", "stream-json"]);
                push(&["--verbose", "--dangerously-skip-permissions"]);
                if let Some(id) = resume {
                    push(&["--resume", id]);
                }
            }
            Harness::Codex { model } => {
                // Stdin stays closed: given a pipe, Codex waits to read more prompt.
                push(&["sh", "-c", r#"exec codex "$@" </dev/null"#, "codex", "exec"]);
                if let Some(id) = resume {
                    push(&["resume", id]);
                }
                push(&["--json", "--skip-git-repo-check"]);
                push(&["--dangerously-bypass-approvals-and-sandbox"]);
                // OpenAI's endpoint, keyed by OPENAI_API_KEY, over plain HTTPS:
                // requests carrying a substituted key cannot be WebSocket upgrades.
                push(&["-c", CODEX_PROVIDER, "-c", "model_provider=smol"]);
                if let Some(model) = model {
                    push(&["--model", model]);
                }
                push(&[prompt]);
            }
            Harness::OpenCode { model } => {
                // Like Codex, OpenCode reads a piped stdin into the prompt.
                push(&["sh", "-c", r#"exec opencode "$@" </dev/null"#, "opencode"]);
                push(&["run", "--format", "json", "--auto", "-m", model]);
                if let Some(id) = resume {
                    push(&["--session", id]);
                }
                push(&[prompt]);
            }
            Harness::Command { program, .. } => {
                cmd.extend(program.iter().cloned());
                cmd.push(prompt.into());
            }
        }
        cmd
    }

    /// Parse one line of the harness's output into events.
    pub fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        match self {
            Harness::ClaudeCode => parse_claude_line(line),
            Harness::Codex { .. } => parse_json_line(line, parse_codex),
            Harness::OpenCode { .. } => parse_json_line(line, parse_opencode),
            Harness::Command { .. } => vec![AgentEvent::Text {
                text: line.to_string(),
            }],
        }
    }
}

/// Run `parse` on a JSON line; anything else the harness printed becomes stderr.
fn parse_json_line(line: &str, parse: fn(&Value) -> Option<Vec<AgentEvent>>) -> Vec<AgentEvent> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(line) {
        Ok(v) => parse(&v).unwrap_or_else(|| vec![AgentEvent::Other(v)]),
        Err(_) => vec![AgentEvent::Stderr {
            line: line.to_string(),
        }],
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `codex exec --json`: thread/turn lifecycle plus items (messages, commands,
/// file edits, MCP and web-search calls) as they start and complete.
fn parse_codex(v: &Value) -> Option<Vec<AgentEvent>> {
    let item = v.get("item").unwrap_or(&Value::Null);
    let events = match (str_at(v, "type")?, str_at(item, "type").unwrap_or("")) {
        ("thread.started", _) => vec![AgentEvent::Started {
            session_id: str_at(v, "thread_id")?.to_string(),
        }],
        ("item.started", "command_execution") => vec![AgentEvent::ToolUse {
            name: "shell".into(),
            input: serde_json::json!({ "command": item.get("command") }),
        }],
        ("item.completed", "command_execution") => vec![AgentEvent::ToolResult {
            output: str_at(item, "aggregated_output").unwrap_or("").to_string(),
            is_error: item
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0),
        }],
        ("item.completed", "agent_message") => vec![AgentEvent::Text {
            text: str_at(item, "text")?.to_string(),
        }],
        ("item.completed", "file_change") => vec![AgentEvent::ToolUse {
            name: "file_change".into(),
            input: serde_json::json!({ "changes": item.get("changes") }),
        }],
        ("item.completed", "mcp_tool_call") => {
            let name = format!(
                "{}.{}",
                str_at(item, "server").unwrap_or("mcp"),
                str_at(item, "tool").unwrap_or("tool")
            );
            let failed = item.get("error").is_some_and(|e| !e.is_null());
            let output = if failed {
                item.get("error")
            } else {
                item.get("result")
            };
            vec![
                AgentEvent::ToolUse {
                    name,
                    input: item.get("arguments").cloned().unwrap_or(Value::Null),
                },
                AgentEvent::ToolResult {
                    output: output.map(value_text).unwrap_or_default(),
                    is_error: failed,
                },
            ]
        }
        ("item.completed", "web_search") => vec![AgentEvent::ToolUse {
            name: "web_search".into(),
            input: serde_json::json!({ "query": item.get("query") }),
        }],
        ("turn.completed", _) => vec![AgentEvent::Finished {
            result: None,
            is_error: false,
            cost_usd: None,
        }],
        ("turn.failed", _) => vec![AgentEvent::Finished {
            result: v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(String::from),
            is_error: true,
            cost_usd: None,
        }],
        // Retries and non-fatal errors: diagnostics, not the agent's output.
        ("error", _) => vec![AgentEvent::Stderr {
            line: str_at(v, "message").unwrap_or("").to_string(),
        }],
        _ => return None,
    };
    Some(events)
}

/// `opencode run --format json`: one line per message part (text, tool call,
/// step boundary), each carrying the session id.
fn parse_opencode(v: &Value) -> Option<Vec<AgentEvent>> {
    let part = v.get("part").unwrap_or(&Value::Null);
    let events = match str_at(v, "type")? {
        "step_start" => vec![AgentEvent::Started {
            session_id: str_at(v, "sessionID")?.to_string(),
        }],
        "text" => vec![AgentEvent::Text {
            text: str_at(part, "text")?.to_string(),
        }],
        "tool_use" => {
            let state = part.get("state").unwrap_or(&Value::Null);
            let failed = str_at(state, "status") == Some("error");
            let output = if failed {
                state.get("error")
            } else {
                state.get("output")
            };
            vec![
                AgentEvent::ToolUse {
                    name: str_at(part, "tool").unwrap_or("tool").to_string(),
                    input: state.get("input").cloned().unwrap_or(Value::Null),
                },
                AgentEvent::ToolResult {
                    output: output.map(value_text).unwrap_or_default(),
                    is_error: failed,
                },
            ]
        }
        // A step that ends for any reason but calling tools ends the turn.
        "step_finish" if str_at(part, "reason") != Some("tool-calls") => {
            vec![AgentEvent::Finished {
                result: None,
                is_error: false,
                cost_usd: None,
            }]
        }
        "error" => vec![AgentEvent::Finished {
            result: v
                .pointer("/error/data/message")
                .or_else(|| v.pointer("/error/name"))
                .and_then(Value::as_str)
                .map(String::from),
            is_error: true,
            cost_usd: None,
        }],
        _ => return None,
    };
    Some(events)
}

/// A JSON value as display text: strings as themselves, anything else as JSON.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
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
    Text {
        /// The text.
        text: String,
    },
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
    Stderr {
        /// The line.
        line: String,
    },
    /// Output the parser did not recognise, kept verbatim.
    Other(Value),
}

fn parse_claude_line(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return if line.trim().is_empty() {
            vec![]
        } else {
            vec![AgentEvent::Text {
                text: line.to_string(),
            }]
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
                Some("text") => {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|t| AgentEvent::Text {
                            text: t.to_string(),
                        })
                }
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
    /// Keep the model API key out of the machine (local target, when the key is
    /// in this process's environment at start). The engine substitutes it on the
    /// way to the provider; the agent only ever sees a placeholder.
    pub key_outside_machine: bool,
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
            key_outside_machine: true,
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
    /// The model API key never enters the machine: the guest holds a placeholder
    /// and the engine substitutes the real key on requests to the provider.
    #[serde(default)]
    pub key_outside_machine: bool,
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
        // Substitution runs in the local engine and reads the real key from the
        // environment the machine boots from — this process's.
        let key_outside_machine = options.key_outside_machine
            && target == "local"
            && options
                .harness
                .api_key_env()
                .is_some_and(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
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
            key_outside_machine,
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
        if key_outside_machine {
            if let (Some(key_env), Some(host)) = (
                options.harness.api_key_env(),
                options.harness.provider_host(),
            ) {
                builder = builder.credential("model", key_env, [host]);
            }
        }
        if !options.open_network {
            for host in options.harness.allowed_hosts() {
                builder = builder.allow_host(host);
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
            let out = machine.exec(["sh", "-c", script.as_str()])?;
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
    /// The harness's API key comes from this process's environment.
    pub fn send(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnRecord> {
        self.send_with_env(prompt, &[], on_event)
    }

    /// [`send`](Self::send) with extra environment for this turn only. A key the
    /// harness needs is taken from `env` first, then from this process's
    /// environment — so a service can pass each caller's key per request.
    pub fn send_with_env(
        &mut self,
        prompt: &str,
        env: &[(String, String)],
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
        for (k, v) in env {
            options = options.env(k.clone(), v.clone());
        }
        let provided = |name: &str| env.iter().any(|(k, _)| k == name);
        if self.record.key_outside_machine {
            // The machine's own environment holds a placeholder for the key and
            // the engine substitutes the real one; passing the key here would
            // put it inside the machine after all.
            if let Some(key_env) = harness.api_key_env().filter(|k| provided(k)) {
                return Err(Error::new(
                    ErrorKind::Config,
                    format!(
                        "session '{}' keeps {key_env} outside its machine; it uses the key \
                         the machine was started with, not one passed per turn",
                        self.record.name
                    ),
                ));
            }
        } else if let Some(key_env) = harness.api_key_env().filter(|k| !provided(k)) {
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
                AgentEvent::Text { text } => texts.push(text.clone()),
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
                            AgentEvent::Stderr {
                                line: line.trim_end().to_string(),
                            },
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
                AgentEvent::Text { text: "Looking.".into() },
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
            vec![AgentEvent::Text {
                text: "not json".into()
            }]
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
        assert_eq!(
            h.parse_line("hi"),
            vec![AgentEvent::Text { text: "hi".into() }]
        );
    }

    // Every event must encode: the CLI's --json output and the service's event
    // files are written this way, and a variant that fails to serialize would
    // silently vanish from both.
    #[test]
    fn codex_json_becomes_agent_events() {
        let h = Harness::Codex { model: None };
        let lines = [
            r#"{"type":"thread.started","thread_id":"01a0d4f4-6279"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"a.txt\n","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2}}"#,
        ];
        let events: Vec<AgentEvent> = lines.iter().flat_map(|l| h.parse_line(l)).collect();
        assert_eq!(
            events[0],
            AgentEvent::Started {
                session_id: "01a0d4f4-6279".into()
            }
        );
        assert!(matches!(events[1], AgentEvent::Other(_)));
        assert!(
            matches!(&events[2], AgentEvent::ToolUse { name, input } if name == "shell" && input["command"] == "ls")
        );
        assert_eq!(
            events[3],
            AgentEvent::ToolResult {
                output: "a.txt\n".into(),
                is_error: false
            }
        );
        assert_eq!(
            events[4],
            AgentEvent::Text {
                text: "done".into()
            }
        );
        assert!(matches!(
            events[5],
            AgentEvent::Finished {
                is_error: false,
                ..
            }
        ));

        // Captured from a real run with an invalid key.
        let failed = h.parse_line(
            r#"{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized: Incorrect API key provided"}}"#,
        );
        assert!(
            matches!(&failed[0], AgentEvent::Finished { is_error: true, result: Some(r), .. } if r.contains("401"))
        );
        let retry = h.parse_line(r#"{"type":"error","message":"Reconnecting... 2/5"}"#);
        assert!(matches!(&retry[0], AgentEvent::Stderr { .. }));
    }

    #[test]
    fn codex_turns_resume_the_previous_thread() {
        let h = Harness::Codex {
            model: Some("gpt-5".into()),
        };
        let cmd = h.turn_command("next", Some("t-1"));
        let exec = cmd.iter().position(|a| a == "exec").unwrap();
        assert_eq!(&cmd[exec..exec + 3], &["exec", "resume", "t-1"]);
        assert!(cmd.iter().any(|a| a.contains("supports_websockets=false")));
        assert!(cmd.contains(&"--json".to_string()));
        assert!(cmd.windows(2).any(|w| w == ["--model", "gpt-5"]));
        assert_eq!(cmd.last().unwrap(), "next");
        assert!(!h
            .turn_command("first", None)
            .contains(&"resume".to_string()));
    }

    #[test]
    fn opencode_json_becomes_agent_events() {
        let h = Harness::OpenCode {
            model: "opencode/big-pickle".into(),
        };
        // Captured from a real `opencode run --format json` turn.
        let lines = [
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_f2b0","part":{"type":"step-start"}}"#,
            r#"{"type":"tool_use","timestamp":2,"sessionID":"ses_f2b0","part":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\n","metadata":{"exit":0}}}}"#,
            r#"{"type":"step_finish","timestamp":3,"sessionID":"ses_f2b0","part":{"type":"step-finish","reason":"tool-calls","cost":0}}"#,
            r#"{"type":"step_start","timestamp":4,"sessionID":"ses_f2b0","part":{"type":"step-start"}}"#,
            r#"{"type":"text","timestamp":5,"sessionID":"ses_f2b0","part":{"type":"text","text":"done"}}"#,
            r#"{"type":"step_finish","timestamp":6,"sessionID":"ses_f2b0","part":{"type":"step-finish","reason":"stop","cost":0}}"#,
        ];
        let events: Vec<AgentEvent> = lines.iter().flat_map(|l| h.parse_line(l)).collect();
        assert_eq!(
            events[0],
            AgentEvent::Started {
                session_id: "ses_f2b0".into()
            }
        );
        assert!(
            matches!(&events[1], AgentEvent::ToolUse { name, input } if name == "bash" && input["command"] == "echo hi")
        );
        assert_eq!(
            events[2],
            AgentEvent::ToolResult {
                output: "hi\n".into(),
                is_error: false
            }
        );
        // A step that stops to call tools does not end the turn.
        assert!(matches!(events[3], AgentEvent::Other(_)));
        assert!(matches!(events[4], AgentEvent::Started { .. }));
        assert_eq!(
            events[5],
            AgentEvent::Text {
                text: "done".into()
            }
        );
        assert!(matches!(
            events[6],
            AgentEvent::Finished {
                is_error: false,
                ..
            }
        ));
        assert_eq!(events.len(), 7);

        let failed = h.parse_line(
            r#"{"type":"error","sessionID":"ses_f2b0","error":{"name":"APIError","data":{"message":"Incorrect API key provided","statusCode":401}}}"#,
        );
        assert!(
            matches!(&failed[0], AgentEvent::Finished { is_error: true, result: Some(r), .. } if r == "Incorrect API key provided")
        );
    }

    #[test]
    fn opencode_provider_decides_the_key() {
        let h = |model: &str| Harness::OpenCode {
            model: model.into(),
        };
        assert_eq!(
            h("anthropic/claude-sonnet-4-5").api_key_env(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(h("openai/gpt-5").api_key_env(), Some("OPENAI_API_KEY"));
        assert_eq!(h("opencode/big-pickle").api_key_env(), None);
        assert!(h("openai/gpt-5")
            .allowed_hosts()
            .contains(&"api.openai.com"));
        let cmd = h("openai/gpt-5").turn_command("go", Some("ses_1"));
        assert!(cmd.windows(2).any(|w| w == ["--session", "ses_1"]));
        assert!(cmd.windows(2).any(|w| w == ["-m", "openai/gpt-5"]));
        assert_eq!(
            Harness::Codex { model: None }.api_key_env(),
            Some("OPENAI_API_KEY")
        );
    }

    #[test]
    fn harnesses_round_trip_through_records() {
        for h in [
            Harness::ClaudeCode,
            Harness::Codex { model: None },
            Harness::OpenCode {
                model: "openai/gpt-5".into(),
            },
        ] {
            let json = serde_json::to_string(&h).unwrap();
            assert_eq!(serde_json::from_str::<Harness>(&json).unwrap(), h);
        }
        assert_eq!(
            serde_json::to_value(Harness::OpenCode {
                model: "x/y".into()
            })
            .unwrap()["kind"],
            "opencode"
        );
    }

    #[test]
    fn every_event_serializes_with_its_type() {
        let events = [
            AgentEvent::Started {
                session_id: "s".into(),
            },
            AgentEvent::Text { text: "t".into() },
            AgentEvent::ToolUse {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            AgentEvent::ToolResult {
                output: "o".into(),
                is_error: false,
            },
            AgentEvent::Finished {
                result: Some("r".into()),
                is_error: false,
                cost_usd: Some(0.1),
            },
            AgentEvent::Stderr { line: "e".into() },
            AgentEvent::Other(serde_json::json!({"x": 1})),
        ];
        for event in events {
            let encoded = serde_json::to_string(&event).expect("event must serialize");
            let back: AgentEvent = serde_json::from_str(&encoded).expect("and round-trip");
            assert_eq!(back, event, "{encoded}");
        }
        assert_eq!(
            serde_json::to_string(&AgentEvent::Text { text: "hi".into() }).unwrap(),
            r#"{"type":"text","text":"hi"}"#
        );
    }

    #[test]
    fn session_names_are_validated() {
        assert!(valid_name("fix-bug-2").is_ok());
        for bad in ["", "Upper", "a b", "-lead", "x/../y"] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
    }
}
