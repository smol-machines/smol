//! Cloud client helper — shared by deploy, machines, destroy, scale commands.
//!
//! Reads the smolfleet endpoint and API key from `~/.config/smolvm/config.toml`
//! under the `[cloud]` section. Performs silent token refresh when expired.

use super::auth;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use smolvm::settings::{CloudSection, SmolSettings};

// ---------------------------------------------------------------------------
// Typed cloud API response structs
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudMachine {
    pub id: String,
    // Optional: a machine row can have a null name (half-created / pool-vended).
    // A single null name must not break parsing the whole list.
    #[serde(default)]
    pub name: Option<String>,
    pub state: String,
    pub source: Option<CloudMachineSource>,
    pub resources: Option<CloudMachineResources>,
    pub network: Option<CloudMachineNetwork>,
    #[serde(default)]
    pub env: Option<serde_json::Value>,
    pub workdir: Option<String>,
    pub ephemeral: Option<bool>,
    pub ttl_seconds: Option<u64>,
    pub auto_stop_seconds: Option<u64>,
    pub last_activity_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Public ingress URL for the machine's first published port, when started
    /// and the control plane advertises a public base URL. `None` otherwise.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct CloudMachineSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub reference: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudMachineResources {
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    pub disk_gb: Option<u32>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct CloudMachineNetwork {
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Common cloud command runner
// ---------------------------------------------------------------------------

/// Execute a cloud command with the common init boilerplate.
///
/// Resolves `name` (name or ID) to a machine ID, then calls `f` with the HTTP
/// client, endpoint URL, and resolved machine ID.
///
/// **Phase 1 (sync) / Phase 2 (async) split**: `cloud_client()` performs a
/// silent token refresh when the stored cloud token is expired, and that
/// refresh path uses `tokio::runtime::Runtime::new() + block_on` internally.
/// If we ran `cloud_client()` inside an active runtime, tokio would panic
/// ("Cannot start a runtime from within a runtime"). Resolving the
/// credentials BEFORE creating the runtime sidesteps this. See
/// `cloud_client_inside_runtime_panics_on_refresh` for the locked-in
/// invariant and `docs/cloud-client-fix-and-cleanup.md` for context.
pub fn run_cloud_command<F, Fut>(name: Option<String>, f: F) -> Result<()>
where
    F: FnOnce(reqwest::Client, String, String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let name_or_id =
        name.ok_or_else(|| anyhow::anyhow!("machine name or ID required for --cloud"))?;

    // Phase 1 (sync): resolve credentials BEFORE entering any runtime.
    let (http, cloud_config) = cloud_client()?;
    let endpoint = cloud_config.endpoint()?.to_string();

    // Phase 2 (async): all network I/O once credentials are settled.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let id = resolve_machine_id(&http, &endpoint, &name_or_id).await?;
        f(http, endpoint, id).await
    })
}

/// Build an HTTP client that includes the API key if configured.
///
/// Attempts silent token refresh if the stored token is expired and a
/// refresh_token is available. Returns the client and cloud section for
/// endpoint access.
pub fn cloud_client() -> Result<(reqwest::Client, CloudSection)> {
    let mut settings = SmolSettings::load()?;

    // Attempt token refresh if expired
    if settings.cloud.is_token_expired() {
        tracing::debug!("cloud token expired, attempting silent refresh");
        if let Some(ref refresh_token) = settings.cloud.refresh_token.clone() {
            match try_refresh(refresh_token) {
                Ok(new_tokens) => {
                    // The smolmachines JWT lives in two places ([cloud] and the
                    // smolmachines registry entry under [machines]). Update both
                    // atomically; otherwise one side silently expires on next use.
                    auth::apply_refreshed_smolmachines_tokens(&mut settings, &new_tokens);
                    let _ = settings.save();
                    tracing::info!("cloud token refreshed");
                    eprintln!("(token refreshed)");
                }
                Err(e) => {
                    anyhow::bail!(
                        "Session expired and refresh failed: {}. Run `smol auth login` to re-authenticate.",
                        e
                    );
                }
            }
        } else {
            anyhow::bail!("Session expired. Run `smol auth login` to re-authenticate.");
        }
    }

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(ref key) = settings.cloud.api_key {
        if key.is_empty() {
            anyhow::bail!("API key is empty. Run `smol auth login` to authenticate.");
        }
        let header_value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", key))
            .map_err(|_| {
                anyhow::anyhow!(
                    "API key contains invalid characters. Run `smol auth login` to re-authenticate."
                )
            })?;
        headers.insert(reqwest::header::AUTHORIZATION, header_value);
    }

    let client = super::common::http_client_builder()
        .default_headers(headers)
        .build()?;

    Ok((client, settings.cloud))
}

/// Resolve a machine name or ID to an ID.
///
/// Tries exact ID match first, then name match. This lets users pass either
/// the full `mach-...` ID or the human-readable name to any cloud command.
pub async fn resolve_machine_id(
    http: &reqwest::Client,
    endpoint: &str,
    name_or_id: &str,
) -> Result<String> {
    let machines = list_machines(http, endpoint).await?;
    for m in &machines {
        if m.id == name_or_id {
            return Ok(name_or_id.to_string());
        }
    }
    for m in &machines {
        if m.name.as_deref() == Some(name_or_id) {
            return Ok(m.id.clone());
        }
    }
    anyhow::bail!("machine '{}' not found", name_or_id);
}

/// Map a non-success cloud response to an actionable error, reading the body
/// for server detail and special-casing auth failures with a re-login hint.
/// Returns the response untouched on success so calls can chain through it.
pub async fn check_response(resp: reqwest::Response, context: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let detail = if body.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", body.trim())
    };
    match status.as_u16() {
        401 => anyhow::bail!(
            "{context}: not authenticated ({status}){detail}. Run `smol auth login` to re-authenticate."
        ),
        403 => anyhow::bail!("{context}: permission denied ({status}){detail}."),
        404 => anyhow::bail!("{context}: not found ({status}){detail}."),
        s if s >= 500 => anyhow::bail!("{context}: control plane error ({status}){detail}."),
        _ => anyhow::bail!("{context}: request failed ({status}){detail}."),
    }
}

/// Fetch the list of all cloud machines.
pub async fn list_machines(http: &reqwest::Client, endpoint: &str) -> Result<Vec<CloudMachine>> {
    // Log method + path only; the Authorization header is never logged.
    tracing::debug!(method = "GET", path = "/v1/machines", %endpoint, "cloud request");
    // GET is idempotent → retry transient blips (the control plane is HA and a
    // momentary 503 / connection reset during a rollover shouldn't fail `smol machine ls`).
    let resp = super::common::send_with_retry(http.get(format!("{}/v1/machines", endpoint)))
        .await
        .with_context(|| format!("could not reach smolfleet control plane at {endpoint}"))?;
    let resp = check_response(resp, "list machines").await?;
    let machines: Vec<CloudMachine> = resp.json().await?;
    tracing::debug!(count = machines.len(), "cloud response: machines listed");
    Ok(machines)
}

/// Attempt a synchronous token refresh using a short-lived tokio runtime.
fn try_refresh(refresh_token: &str) -> Result<auth::TokenResponse> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(auth::refresh_access_token(refresh_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Documents a load-bearing constraint: `try_refresh` (and the
    /// `cloud_client()` path that calls it) must NOT be invoked from inside
    /// an active tokio runtime. The refresh uses `Runtime::new() + block_on`
    /// internally, and tokio panics if that's nested under another runtime.
    ///
    /// Callers MUST resolve cloud credentials sync, BEFORE entering any
    /// tokio runtime. `run_cloud_command`, `machines::run`, `deploy::run`
    /// all follow this pattern; the `#[should_panic]` lock encodes the
    /// invariant they exist to maintain.
    ///
    /// If this test stops panicking (e.g., `try_refresh` was refactored to
    /// be async-aware via `block_in_place`, or made `async fn`), the
    /// constraint has been removed and the Phase-1 boilerplate in callers
    /// can be dropped.
    #[tokio::test]
    #[should_panic(expected = "Cannot start a runtime from within a runtime")]
    async fn try_refresh_inside_runtime_panics() {
        // Inside `#[tokio::test]` we're already inside a runtime. The inner
        // Runtime::new() + block_on in `try_refresh` panics during scheduler
        // start — no HTTP request is ever issued.
        let _ = try_refresh("any_token_value");
    }

    #[test]
    fn cloud_machine_deserializes_full_response() {
        let json = r#"{
            "id": "mach-xxx",
            "name": "test",
            "state": "started",
            "source": {"type": "image", "reference": "alpine"},
            "resources": {"cpus": 1, "memoryMb": 256, "diskGb": null},
            "network": {"mode": "blocked"},
            "env": {},
            "createdAt": "2026-05-28T00:00:00Z",
            "updatedAt": "2026-05-28T01:00:00Z"
        }"#;

        let m: CloudMachine = serde_json::from_str(json).unwrap();
        assert_eq!(m.id, "mach-xxx");
        assert_eq!(m.name.as_deref(), Some("test"));
        assert_eq!(m.state, "started");

        let source = m.source.unwrap();
        assert_eq!(source.source_type, "image");
        assert_eq!(source.reference.as_deref(), Some("alpine"));

        let resources = m.resources.unwrap();
        assert_eq!(resources.cpus, Some(1));
        assert_eq!(resources.memory_mb, Some(256));
        assert_eq!(resources.disk_gb, None);

        let network = m.network.unwrap();
        assert_eq!(network.mode.as_deref(), Some("blocked"));

        assert_eq!(m.created_at.as_deref(), Some("2026-05-28T00:00:00Z"));
        assert_eq!(m.updated_at.as_deref(), Some("2026-05-28T01:00:00Z"));
    }

    #[test]
    fn cloud_machine_deserializes_minimal_response() {
        let json = r#"{"id": "mach-1", "name": "bare", "state": "stopped"}"#;
        let m: CloudMachine = serde_json::from_str(json).unwrap();
        assert_eq!(m.id, "mach-1");
        assert_eq!(m.name.as_deref(), Some("bare"));
        assert_eq!(m.state, "stopped");
        assert!(m.source.is_none());
        assert!(m.resources.is_none());
        assert!(m.network.is_none());
        assert!(m.created_at.is_none());
        assert!(m.updated_at.is_none());
    }

    #[test]
    fn cloud_machine_list_deserializes() {
        let json = r#"[
            {"id": "mach-1", "name": "a", "state": "started"},
            {"id": "mach-2", "name": "b", "state": "stopped", "source": {"type": "smolmachine", "reference": "myapp:v1"}}
        ]"#;
        let machines: Vec<CloudMachine> = serde_json::from_str(json).unwrap();
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].name.as_deref(), Some("a"));
        assert_eq!(
            machines[1].source.as_ref().unwrap().source_type,
            "smolmachine"
        );
    }
}

// ---------------------------------------------------------------------------
// `smol cloud` command group — smolfleet (cloud) operations.
// ---------------------------------------------------------------------------

/// Manage machines deployed on the smolfleet cloud.
#[derive(Args, Debug)]
pub struct CloudCmd {
    #[command(subcommand)]
    pub command: CloudSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum CloudSubcommand {
    /// Deploy a machine to smolfleet
    Deploy(crate::commands::deploy::DeployCmd),

    /// List deployed machines on smolfleet
    Ls(crate::commands::machines::MachinesCmd),

    /// Destroy a deployed machine on smolfleet
    Rm(crate::commands::destroy::DestroyCmd),

    /// Scale a machine on smolfleet
    Scale(crate::commands::scale::ScaleCmd),

    /// Open an interactive shell on a deployed cloud machine
    #[command(visible_alias = "sh")]
    Shell {
        /// Machine name (default: "default")
        #[arg(short = 'n', long, value_name = "NAME")]
        name: Option<String>,
    },
}

impl CloudCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            CloudSubcommand::Deploy(cmd) => cmd.run(),
            CloudSubcommand::Ls(cmd) => cmd.run(),
            CloudSubcommand::Rm(cmd) => cmd.run(),
            CloudSubcommand::Scale(cmd) => cmd.run(),
            CloudSubcommand::Shell { name } => crate::commands::exec::ExecCmd {
                name,
                command: vec!["/bin/sh".to_string()],
                interactive: true,
                tty: true,
                stream: false,
                env: vec![],
                workdir: None,
                secret_env: vec![],
                secret_file: vec![],
                timeout: None,
                cloud: true,
                local: false,
            }
            .run(),
        }
    }
}
