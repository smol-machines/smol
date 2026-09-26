//! The smolfleet `/v1` wire shapes.
//!
//! Field names are camelCase on the wire and snake_case here. Nearly
//! everything is optional: a machine row can be half-created, an older control
//! plane omits fields a newer one sends, and a single missing value must never
//! fail the parse of a whole list.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Create a managed agent session on smol cloud.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgent {
    /// Unique session name within the account.
    pub name: String,
    /// `claude-code`, `codex`, `opencode`, or `command`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Harness model, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Image for a command harness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Program for a command harness.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub program: Vec<String>,
    /// Extra egress hosts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
    /// Permit unrestricted outbound traffic.
    pub open_network: bool,
    /// Capture a checkpoint after each completed turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoints: Option<bool>,
    /// Virtual CPUs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    /// Guest memory in MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// Requested CPU architecture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Name of a stored provider credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// One turn and its final outcome, if available.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTurn {
    /// Zero-based turn index.
    pub index: u64,
    /// Prompt submitted for this turn.
    pub prompt: String,
    /// `preparing`, `running`, `done`, `failed`, `cancelled`, or `interrupted`.
    pub status: String,
    /// Final answer or failure reason.
    pub result: Option<String>,
    /// Whether the harness reported an error.
    pub is_error: bool,
    /// Provider cost when reported by the harness.
    pub cost_usd: Option<f64>,
    /// Whether this turn can be a rewind or fork point.
    pub checkpointed: bool,
    /// When the turn started.
    pub started_at: String,
    /// When the turn finished.
    pub finished_at: Option<String>,
}

/// One managed agent session.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    /// Session name.
    pub name: String,
    /// Harness name.
    pub harness: String,
    /// Model, if configured.
    pub model: Option<String>,
    /// Session lifecycle state.
    pub status: String,
    /// Failure reason, if any.
    pub error: Option<String>,
    /// Current machine id.
    pub machine_id: Option<String>,
    /// Running turn index.
    pub running_turn: Option<u64>,
    /// Retained turn history.
    pub turns: Vec<AgentTurn>,
    /// When the session was created.
    pub created_at: String,
}

/// Compact entry in the paginated session list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    /// Session name.
    pub name: String,
    /// Harness name.
    pub harness: String,
    /// Model, if configured.
    pub model: Option<String>,
    /// Session lifecycle state.
    pub status: String,
    /// Failure reason, if any.
    pub error: Option<String>,
    /// Current machine id.
    pub machine_id: Option<String>,
    /// Running turn index.
    pub running_turn: Option<u64>,
    /// When the session was created.
    pub created_at: String,
}

/// One page of managed agent sessions.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPage {
    /// Session summaries.
    pub items: Vec<AgentSummary>,
    /// Name cursor for the following page.
    pub next_cursor: Option<String>,
}

/// Input for one managed agent turn.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAgentTurn {
    /// Task prompt.
    pub prompt: String,
    /// Environment for this turn only.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Maximum run time in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// A turn accepted for asynchronous execution.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentTurnAccepted {
    /// Zero-based turn index.
    pub turn: u64,
}

/// A published port, and the node host port the control plane allocated for it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Port {
    /// Port the guest serves on.
    pub port: u16,
    /// Port allocated on the node. `None` until the machine is started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
}

/// What a machine boots from.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Source {
    /// Source kind, `image` or `smolmachine`.
    #[serde(rename = "type")]
    pub source_type: String,
    /// Image reference or pack name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// CPU architecture, `arm64` or `amd64`.
    ///
    /// Requested on the way in, reported on the way out. Leaving it unset lets
    /// the control plane place the machine on whatever it has, which is usually
    /// what you want — set it when something downstream cares, such as a
    /// checkpoint, which only restores on the architecture it was taken on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

/// What a machine was given.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    /// Virtual CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Guest RAM in MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// Data disk in GB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u32>,
}

/// A machine's egress policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Network {
    /// `open`, `allowCidrs`, or none at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// CIDRs the machine may reach, under `allowCidrs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cidrs: Vec<String>,
    /// Hostnames the machine may reach, under `allowCidrs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

/// One machine, as the control plane describes it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// Control-plane id. This, not the name, addresses the machine.
    pub id: String,
    /// Caller-chosen name. A half-created or pool-vended row can have none.
    #[serde(default)]
    pub name: Option<String>,
    /// Lifecycle state. `started` means the VM process launched — it does *not*
    /// mean the guest agent or workload is up. See [`Machine::ready`].
    pub state: String,
    /// True once the guest agent answers and any published port accepts. This,
    /// not `state`, is what "ready for work" means.
    #[serde(default)]
    pub ready: Option<bool>,
    /// When readiness was first observed.
    #[serde(default)]
    pub ready_at: Option<String>,
    #[serde(default)]
    /// What the machine boots from.
    pub source: Option<Source>,
    #[serde(default)]
    /// What the machine was given.
    pub resources: Option<Resources>,
    #[serde(default)]
    /// The machine's egress policy.
    pub network: Option<Network>,
    /// Published ports.
    #[serde(default)]
    pub ports: Vec<Port>,
    #[serde(default)]
    /// Workload environment, as the server stored it.
    pub env: Option<serde_json::Value>,
    #[serde(default)]
    /// Workload working directory.
    pub workdir: Option<String>,
    #[serde(default)]
    /// Whether the machine is discarded when it stops.
    pub ephemeral: Option<bool>,
    #[serde(default)]
    /// Hard lifetime, after which the machine is deleted.
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    /// Idle seconds before the machine stops on its own.
    pub auto_stop_seconds: Option<u64>,
    #[serde(default)]
    /// When the machine was last used.
    pub last_activity_at: Option<String>,
    #[serde(default)]
    /// When the machine was created.
    pub created_at: Option<String>,
    #[serde(default)]
    /// When the record last changed.
    pub updated_at: Option<String>,
    /// Public ingress URL for the first published port, when the control plane
    /// advertises a public base URL.
    #[serde(default)]
    pub url: Option<String>,
}

impl Machine {
    /// The name if it has one, otherwise the id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// Whether the machine is ready to do work.
    ///
    /// An older control plane omits `ready` entirely; there, a machine that
    /// has started is the best signal available.
    pub fn is_ready(&self) -> bool {
        match self.ready {
            Some(ready) => ready,
            None => self.is_started(),
        }
    }

    /// Whether the VM process has launched.
    ///
    /// This is *not* readiness: the guest is still booting, and the agent has
    /// not answered. Keep the two apart — a machine can be started and not
    /// ready for a long time, which is exactly when the readiness fallbacks
    /// matter.
    pub fn is_started(&self) -> bool {
        matches!(self.state.as_str(), "started" | "running")
    }

    /// Whether waiting could still make this machine ready.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "error" | "stopped" | "deleted")
    }
}

/// What to create.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMachine {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Name for the machine. The server generates one when absent.
    pub name: Option<String>,
    /// What to boot. Required in practice.
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Sizing. Unset fields take the server's defaults.
    pub resources: Option<Resources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Egress policy. Unset means the server's default.
    pub network: Option<Network>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    /// Ports to publish. Supply only the guest port; the control plane
    /// allocates the node host port.
    pub ports: Vec<Port>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Workload environment.
    pub env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Workload working directory.
    pub workdir: Option<String>,
    /// Command overriding the image entrypoint. Empty keeps the image default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Idle seconds before the machine stops on its own.
    pub auto_stop_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Hard lifetime, after which the machine is deleted.
    pub ttl_seconds: Option<u64>,
    /// Branchability is a create-time property: the control plane persists it
    /// and the branch endpoint checks the stored flag. Sending it only at start
    /// time stores the source non-branchable and every branch 409s.
    ///
    /// Send this name only. The server accepts the older `forkable` as an alias
    /// for the same field, so a body carrying both is rejected outright as a
    /// duplicate — they are two spellings of one thing, not two fields.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub branchable: bool,
}

/// A command to run in a machine.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    /// argv, not a shell string.
    pub command: Vec<String>,
    /// Extra environment for this command.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Server-side timeout.
    pub timeout_seconds: Option<u64>,
}

/// What a finished command produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    #[serde(default)]
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// Text stdout, capped at 1 MiB by the server.
    #[serde(default)]
    pub stdout: Option<String>,
    /// Text stderr, capped at 1 MiB by the server.
    #[serde(default)]
    pub stderr: Option<String>,
    /// Byte-exact, untruncated stdout. Prefer this where present.
    #[serde(default)]
    pub stdout_b64: Option<String>,
    /// Byte-exact, untruncated stderr. Prefer this where present.
    #[serde(default)]
    pub stderr_b64: Option<String>,
    #[serde(default)]
    /// Whether the text stdout hit the server's cap.
    pub stdout_truncated: Option<bool>,
    #[serde(default)]
    /// Whether the text stderr hit the server's cap.
    pub stderr_truncated: Option<bool>,
}

impl CommandOutput {
    /// Stdout as bytes, preferring the untruncated base64 field.
    pub fn stdout_bytes(&self) -> Vec<u8> {
        decode(self.stdout_b64.as_deref(), self.stdout.as_deref())
    }

    /// Stderr as bytes, preferring the untruncated base64 field.
    pub fn stderr_bytes(&self) -> Vec<u8> {
        decode(self.stderr_b64.as_deref(), self.stderr.as_deref())
    }
}

fn decode(b64: Option<&str>, text: Option<&str>) -> Vec<u8> {
    use base64::Engine as _;
    b64.and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
        .unwrap_or_else(|| text.unwrap_or_default().as_bytes().to_vec())
}

/// A stored capture of a machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    /// Capture id.
    pub id: String,
    /// The machine this captures.
    pub machine_id: String,
    /// Capture status, `ready` once it can be restored from.
    pub status: String,
    /// Size of the stored artifact.
    pub size_bytes: u64,
    /// Architecture the capture was taken on. A capture only restores on its
    /// own architecture.
    pub arch: String,
    /// When the capture was taken.
    pub created_at: String,
    /// Pre-signed URL to download the artifact.
    #[serde(default)]
    pub download_url: Option<String>,
}

/// What a batch branch returned.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BranchBatch {
    /// The clones the call created.
    #[serde(default)]
    pub clones: Vec<Machine>,
}

/// A shareable link to a machine.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    /// Bearer token for the link.
    pub token: String,
    /// Full URL, absent when there is no apps domain or the name is not
    /// DNS-safe. The token can still be attached as `?t=`.
    #[serde(default)]
    pub url: Option<String>,
}

/// Metered usage for one machine.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageTotals {
    /// Seconds the machine was running.
    pub total_uptime_seconds: f64,
    /// vCPU-hours consumed.
    pub cpu_hours: f64,
    /// GB-hours of guest RAM.
    pub memory_gb_hours: f64,
    /// GB-hours of disk.
    pub disk_gb_hours: f64,
    /// Gigabytes sent out.
    pub egress_gb: f64,
}

/// Cost for one machine, in micro-dollars (1e-6 USD).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CostBreakdown {
    /// CPU charge.
    pub cpu_micros: i64,
    /// Memory charge.
    pub memory_micros: i64,
    /// Disk charge.
    pub disk_micros: i64,
    /// Egress charge.
    pub egress_micros: i64,
    /// Flat charge for the machine existing.
    pub base_micros: i64,
    /// Everything above, added up.
    pub total_micros: i64,
    /// What is actually owed after credit.
    pub amount_due_micros: i64,
}

/// Usage and cost for one machine over a window.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// The machine this reports on.
    pub machine_id: String,
    /// Start of the reported window.
    pub from: String,
    /// End of the reported window.
    pub to: String,
    #[serde(default)]
    /// Metered totals for the window.
    pub usage: UsageTotals,
    #[serde(default)]
    /// What those totals cost.
    pub cost: CostBreakdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_row_with_a_null_name_still_parses() {
        let machine: Machine =
            serde_json::from_str(r#"{"id":"m-1","name":null,"state":"started"}"#)
                .expect("a half-created row must not fail the whole list");
        assert_eq!(machine.display_name(), "m-1");
        assert!(machine.ports.is_empty());
    }

    #[test]
    fn an_older_control_plane_without_ready_falls_back_to_state() {
        let started: Machine =
            serde_json::from_str(r#"{"id":"m","state":"started"}"#).expect("parse");
        assert!(started.is_ready());
        let booting: Machine =
            serde_json::from_str(r#"{"id":"m","state":"creating"}"#).expect("parse");
        assert!(!booting.is_ready());
        // An explicit false always wins over the state guess.
        let not_yet: Machine =
            serde_json::from_str(r#"{"id":"m","state":"started","ready":false}"#).expect("parse");
        assert!(!not_yet.is_ready());
    }

    #[test]
    fn untruncated_bytes_win_over_the_capped_text() {
        let output = CommandOutput {
            stdout: Some("tru".into()),
            stdout_b64: Some("dHJ1bmNhdGVk".into()),
            ..Default::default()
        };
        assert_eq!(output.stdout_bytes(), b"truncated".to_vec());
        // With no base64 field the text is all there is.
        let text_only = CommandOutput {
            stdout: Some("plain".into()),
            ..Default::default()
        };
        assert_eq!(text_only.stdout_bytes(), b"plain".to_vec());
    }

    #[test]
    fn an_unset_create_field_is_left_out_so_the_server_defaults_it() {
        let body = serde_json::to_value(CreateMachine {
            name: Some("m".into()),
            source: Some(Source {
                source_type: "image".into(),
                reference: Some("alpine".into()),
                arch: None,
            }),
            ..Default::default()
        })
        .expect("serialize");
        assert_eq!(body["name"], "m");
        assert_eq!(body["source"]["type"], "image");
        assert!(body.get("ttlSeconds").is_none());
        assert!(body.get("ports").is_none());
        // False must not be sent either: it is the absence that means default.
        assert!(body.get("branchable").is_none());
        assert!(body.get("command").is_none());
    }

    #[test]
    fn a_branchable_machine_is_asked_for_by_one_name_only() {
        let request = CreateMachine {
            branchable: true,
            ..Default::default()
        };
        let body = serde_json::to_value(&request).expect("serialize");
        assert_eq!(body["branchable"], true);
        // `forkable` is the server's alias for the same field, so sending both
        // is a duplicate-field 422 that fails every create.
        assert!(body.get("forkable").is_none());
    }

    #[test]
    fn an_architecture_rides_on_the_source() {
        let body = serde_json::to_value(Source {
            source_type: "image".into(),
            reference: Some("alpine".into()),
            arch: Some("arm64".into()),
        })
        .expect("serialize");
        assert_eq!(body["arch"], "arm64");
        // Unset means "place it wherever", not "amd64".
        let unset = serde_json::to_value(Source {
            source_type: "image".into(),
            reference: Some("alpine".into()),
            arch: None,
        })
        .expect("serialize");
        assert!(unset.get("arch").is_none());
    }
}
