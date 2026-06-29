//! Shared helpers for smol CLI commands.

use smolvm::agent::{AgentClient, AgentManager};

/// How long to wait for a TCP connection to a cloud/auth endpoint before
/// giving up. Without this, a black-hole host (accepts SYN, never responds)
/// makes `smol auth login` / `smol deploy` hang forever.
pub const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Overall per-request deadline for cloud/auth HTTP calls. Generous enough
/// for a slow control-plane operation, short enough that a stuck request
/// surfaces as an error instead of an indefinite hang.
pub const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Build a reqwest client preconfigured with connect + request timeouts.
/// All cloud- and auth-facing HTTP should go through this so no call can
/// hang indefinitely. The returned builder lets callers add headers before
/// `.build()`.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
}

/// A default timeout-configured client for callers that need no extra config.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    http_client_builder().build()
}

/// Max send attempts for an idempotent cloud request (1 try + 2 retries).
const HTTP_MAX_ATTEMPTS: u32 = 3;

/// HTTP statuses worth retrying: transient infra failures that clear on a retry —
/// 429 (rate limited), 502/503/504 (bad gateway / unavailable / gateway timeout).
/// A 500 is left alone (it may be a deterministic server bug; retrying just spams).
pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

/// reqwest errors worth retrying: a connection that didn't establish or a request
/// that timed out — the control plane was briefly unreachable or slow. A
/// body/decode error is not retried.
pub fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

/// Exponential backoff before retry attempt `n` (1-based): 200ms, 400ms, 800ms…
fn retry_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(200u64.saturating_mul(1u64 << (attempt - 1)))
}

/// Send an IDEMPOTENT request, retrying transient failures (see
/// [`is_retryable_status`] / [`is_retryable_error`]) with exponential backoff.
/// Only retries when the request is cloneable (no streaming body); a
/// non-replayable body is sent exactly once. The caller MUST ensure the request
/// is idempotent — a retried non-idempotent POST could double-execute, so this is
/// for GETs / safe reads, not create/exec.
pub async fn send_with_retry(req: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let mut attempt: u32 = 1;
    loop {
        // `try_clone` is None for a non-replayable (streaming) body → send once.
        let Some(attempt_req) = req.try_clone() else {
            return req.send().await;
        };
        match attempt_req.send().await {
            Ok(resp) if is_retryable_status(resp.status()) && attempt < HTTP_MAX_ATTEMPTS => {}
            Ok(resp) => return Ok(resp),
            Err(e) if is_retryable_error(&e) && attempt < HTTP_MAX_ATTEMPTS => {}
            Err(e) => return Err(e),
        }
        tokio::time::sleep(retry_backoff(attempt)).await;
        attempt += 1;
    }
}

/// Validate a user-supplied machine name before it derives on-disk state paths.
/// Defense-in-depth at the CLI boundary (the engine also rejects separators):
/// allow only `[A-Za-z0-9._-]`, 1-63 chars, no leading dot / `..` traversal.
pub fn validate_machine_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 63 {
        anyhow::bail!("machine name must be 1-63 characters (got {})", name.len());
    }
    if name == ".." || name.starts_with('.') {
        anyhow::bail!("machine name '{name}' is invalid — must not start with '.' or be '..'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        anyhow::bail!("machine name '{name}' is invalid — use only letters, digits, '-', '_', '.'");
    }
    Ok(())
}

/// Get an AgentManager for the given name, defaulting to the canonical "default" VM.
pub fn get_manager(name: &str) -> anyhow::Result<AgentManager> {
    validate_machine_name(name)?;
    Ok(if name == "default" {
        AgentManager::new_default()?
    } else {
        AgentManager::for_vm(name)?
    })
}

/// Resolve machine name, defaulting to "default".
pub fn resolve_name(name: Option<String>) -> String {
    name.unwrap_or_else(|| "default".to_string())
}

/// Parse `--secret-env GUEST=HOST_VAR` / `--secret-file GUEST=/abs/path` specs
/// into validated [`SecretRef`]s keyed by guest var. CLI-supplied refs are
/// `TrustedLocal` (the host user invoked the command).
pub fn parse_cli_secret_refs(
    secret_env: &[String],
    secret_file: &[String],
) -> anyhow::Result<std::collections::BTreeMap<String, smolvm::secrets::SecretRef>> {
    use smolvm::secrets::{env_ref, file_ref, validate_ref, ResolutionScope, SecretRef};
    use std::collections::BTreeMap;

    let mut refs: BTreeMap<String, SecretRef> = BTreeMap::new();
    let mut add = |flag: &str, spec: &str, make: &dyn Fn(&str) -> SecretRef| -> anyhow::Result<()> {
        let (key, value) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("{flag}: expected KEY=VALUE, got '{spec}'"))?;
        if key.is_empty() {
            anyhow::bail!("{flag}: empty secret name in '{spec}'");
        }
        let r = make(value);
        validate_ref(&r, ResolutionScope::TrustedLocal)
            .map_err(|e| anyhow::anyhow!("{flag}: secret '{key}': {e}"))?;
        if refs.insert(key.to_string(), r).is_some() {
            anyhow::bail!("{flag}: secret '{key}' specified more than once");
        }
        Ok(())
    };
    for spec in secret_env {
        add("--secret-env", spec, &|v| env_ref(v))?;
    }
    for spec in secret_file {
        add("--secret-file", spec, &|v| file_ref(v))?;
    }
    Ok(refs)
}

/// Parse CLI secret specs and resolve them host-side into `(guest_var,
/// plaintext)` env pairs for a single exec. Plaintext stays in the returned
/// vector — never persisted. Scope `TrustedLocal`.
pub fn resolve_cli_secrets(
    secret_env: &[String],
    secret_file: &[String],
) -> anyhow::Result<Vec<(String, String)>> {
    let refs = parse_cli_secret_refs(secret_env, secret_file)?;
    let resolved =
        smolvm::secrets::resolve_refs_to_env(&refs, smolvm::secrets::ResolutionScope::TrustedLocal)?;
    Ok(smolvm::secrets::expose_into_env(resolved))
}

/// Resolve a machine record's stored `secret_refs` (written at create time) into
/// `(guest_var, plaintext)` env pairs at launch/exec. Scope `RecordReplay`.
pub fn resolve_record_secrets(
    refs: &std::collections::BTreeMap<String, smolvm::secrets::SecretRef>,
) -> anyhow::Result<Vec<(String, String)>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let resolved =
        smolvm::secrets::resolve_refs_to_env(refs, smolvm::secrets::ResolutionScope::RecordReplay)?;
    Ok(smolvm::secrets::expose_into_env(resolved))
}

/// Format a byte count as a human-readable size (1024-based).
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Derive machine name from the current directory name.
pub fn name_from_cwd() -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    let dir_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    Ok(dir_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect())
}

/// Resolved credentials for a registry, ready to be applied to a `RegistryClient`.
enum ResolvedCredential {
    /// Auth0 JWT or similar upstream identity credential.
    /// Exchanged with the registry's token service per-operation to obtain a
    /// short-lived OCI bearer token. Only this variant is eligible for silent
    /// OAuth refresh via `smol auth login`'s stored `refresh_token`.
    Identity(String),
    /// Static bearer token (legacy `username="token"` convention).
    /// Sent directly as `Authorization: Bearer` on every request.
    Direct(String),
    /// Standard Docker/OCI login credentials.
    /// Sent as `Authorization: Basic` to the registry's token endpoint
    /// after a `WWW-Authenticate: Bearer` challenge. Used for Docker Hub,
    /// GHCR, ECR, GCR, ACR, Harbor, and Quay.
    Basic { username: String, password: String },
}

/// Build a `RegistryClient` from a registry hostname, applying auth and mirror config.
///
/// Silently refreshes an expired identity token if a `refresh_token` is stored.
/// On successful refresh the new tokens are persisted to `~/.config/smolvm/config.toml`
/// so subsequent invocations don't need to re-authenticate.
pub fn build_registry_client(
    registry: &str,
    config: &smolvm::registry::RegistryConfig,
    cloud: &smolvm::settings::CloudSection,
) -> anyhow::Result<smolvm_registry::RegistryClient> {
    let effective_registry = config.get_mirror(registry).unwrap_or(registry);

    // Docker Hub: user-facing name is "docker.io" but the Distribution API
    // endpoint is "registry-1.docker.io". Config key stays "docker.io".
    let api_host = match effective_registry {
        "docker.io" => "registry-1.docker.io",
        h => h,
    };

    let base_url = if smolvm_registry::is_local_registry(api_host) {
        format!("http://{}", api_host)
    } else {
        format!("https://{}", api_host)
    };

    tracing::debug!(registry, resolved_host = api_host, base_url = %base_url, "resolved registry host");

    let mut client = smolvm_registry::RegistryClient::new(base_url);

    // Credentials are stored by `smol auth login`. Identity tokens are refreshed
    // here if expired. Route based on credential type, not registry hostname,
    // so self-hosted registries using the identity_token path work correctly.
    match resolve_token(registry, config, cloud)? {
        Some(ResolvedCredential::Identity(token)) => {
            client = client.with_identity_token(token);
        }
        Some(ResolvedCredential::Direct(token)) => {
            client = client.with_token(token);
        }
        Some(ResolvedCredential::Basic { username, password }) => {
            client = client.with_basic_credentials(username, password);
        }
        None => {}
    }

    Ok(client)
}

/// Resolve credentials for `registry` from config, refreshing if expired.
///
/// Returns `None` when no credentials are configured for the registry.
/// Returns the stored credentials unchanged when they have no expiry set.
/// Only `ResolvedCredential::Identity` is eligible for silent OAuth refresh.
fn resolve_token(
    registry: &str,
    config: &smolvm::registry::RegistryConfig,
    cloud: &smolvm::settings::CloudSection,
) -> anyhow::Result<Option<ResolvedCredential>> {
    // The smolmachines registry's credential IS the cloud Auth0 session — the
    // registry token endpoint (`/v2/auth`) authenticates that same JWT. Read it
    // LIVE from `[cloud]` (single source of truth) instead of a separate copy in
    // `[machines.registries]`: a partial login/refresh (an older binary, or a
    // `/v1`-only refresh) can leave that copy stale → push/pull 401. Other
    // registries (docker.io, private, self-hosted) keep their own independent
    // per-registry credentials via the entry logic below.
    if registry == smolvm::registry::SMOLMACHINES_REGISTRY {
        if let Some(cred) = resolve_smolmachines_cloud_token(cloud)? {
            return Ok(Some(cred));
        }
        // No cloud session configured → fall through to any manual registry entry.
    }

    let entry = match config.registries.get(registry) {
        Some(e) => e,
        None => return Ok(None),
    };

    // identity_token (e.g. Auth0 JWT) takes precedence.
    // Direct/Basic credentials are static — they don't expire via OAuth refresh.
    if let Some(identity_token) = &entry.identity_token {
        return resolve_identity_token(registry, identity_token, entry);
    }

    if let Some(auth) = config.get_credentials(registry) {
        if auth.username == "token" {
            return Ok(Some(ResolvedCredential::Direct(auth.password)));
        } else {
            return Ok(Some(ResolvedCredential::Basic {
                username: auth.username,
                password: auth.password,
            }));
        }
    }

    Ok(None)
}

/// Resolve the smolmachines registry credential from the `[cloud]` Auth0 session
/// — the single source of truth — refreshing it if expired. Returns `None` when
/// there is no cloud session (caller falls back to any manual registry entry).
///
/// On a successful refresh, [`apply_refreshed_smolmachines_tokens`] re-syncs the
/// `[machines.registries]` copy so both stay current. The network refresh runs
/// ONLY when the cloud token is actually expired — the common warm path is a
/// borrow + clone with no I/O, which also keeps the unit tests offline.
///
/// [`apply_refreshed_smolmachines_tokens`]: super::auth::apply_refreshed_smolmachines_tokens
fn resolve_smolmachines_cloud_token(
    cloud: &smolvm::settings::CloudSection,
) -> anyhow::Result<Option<ResolvedCredential>> {
    let token = match &cloud.api_key {
        Some(t) => t.clone(),
        None => return Ok(None),
    };
    if !cloud.is_token_expired() {
        return Ok(Some(ResolvedCredential::Identity(token)));
    }

    // Expired (or within the refresh buffer): silently refresh via the cloud
    // refresh token. Without one, surface the stale token so the registry
    // returns a clear 401 with a "run `smol auth login`" hint.
    let refresh_token = match &cloud.refresh_token {
        Some(rt) => rt.clone(),
        None => {
            eprintln!("warning: cloud session is expired. Run `smol auth login` to re-authenticate.");
            return Ok(Some(ResolvedCredential::Identity(token)));
        }
    };

    eprintln!("Refreshing expired cloud session...");
    let rt = tokio::runtime::Runtime::new()?;
    let new_tokens = rt
        .block_on(super::auth::refresh_access_token(&refresh_token))
        .map_err(|e| {
            anyhow::anyhow!(
                "token refresh failed: {}. Run `smol auth login` to re-authenticate.",
                e
            )
        })?;

    // Persist + keep [cloud] and the [machines.registries] copy in lockstep.
    let mut settings = smolvm::SmolSettings::load()?;
    super::auth::apply_refreshed_smolmachines_tokens(&mut settings, &new_tokens);
    settings.save()?;

    Ok(Some(ResolvedCredential::Identity(new_tokens.access_token)))
}

/// Handle expiry check and silent refresh for identity tokens (Auth0 JWTs).
/// Direct bearer and Basic credentials are not eligible for Auth0 refresh.
fn resolve_identity_token(
    registry: &str,
    identity_token: &str,
    entry: &smolvm::registry::RegistryEntry,
) -> anyhow::Result<Option<ResolvedCredential>> {
    // No expiry recorded — token was supplied manually; use as-is.
    let expires_at = match entry.expires_at {
        Some(t) => t,
        None => {
            tracing::debug!(registry, "using identity token with no recorded expiry");
            return Ok(Some(ResolvedCredential::Identity(
                identity_token.to_string(),
            )));
        }
    };

    // Buffer of 60 seconds: refresh slightly before hard expiry.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if now < expires_at - 60 {
        tracing::debug!(
            registry,
            expires_at,
            "using cached identity token (not yet expired)"
        );
        return Ok(Some(ResolvedCredential::Identity(
            identity_token.to_string(),
        )));
    }

    // Token is expired (or about to be). Try a silent refresh.
    tracing::debug!(
        registry,
        expires_at,
        now,
        "identity token expired, attempting silent refresh"
    );
    let refresh_token = match &entry.refresh_token {
        Some(rt) => rt.clone(),
        None => {
            tracing::warn!(
                registry,
                "token expired and no refresh token available; using stale token"
            );
            eprintln!(
                "warning: token for {} is expired. Run `smol auth login` to re-authenticate.",
                registry
            );
            // Return the stale token and let the registry return a clear 401.
            return Ok(Some(ResolvedCredential::Identity(
                identity_token.to_string(),
            )));
        }
    };

    eprintln!("Refreshing expired token for {}...", registry);
    let rt = tokio::runtime::Runtime::new()?;
    let new_tokens = rt
        .block_on(super::auth::refresh_access_token(&refresh_token))
        .map_err(|e| {
            anyhow::anyhow!(
                "token refresh failed: {}. Run `smol auth login` to re-authenticate.",
                e
            )
        })?;

    // Persist the refreshed tokens so the next invocation is also fast.
    // For the smolmachines registry the JWT also lives in [cloud] and must
    // stay in lockstep — see apply_refreshed_smolmachines_tokens.
    let mut settings = smolvm::SmolSettings::load()?;
    if registry == smolvm::registry::SMOLMACHINES_REGISTRY {
        super::auth::apply_refreshed_smolmachines_tokens(&mut settings, &new_tokens);
    } else if let Some(entry) = settings.machines.registries.get_mut(registry) {
        super::auth::apply_refreshed_registry_tokens(entry, &new_tokens);
    }
    settings.save()?;

    Ok(Some(ResolvedCredential::Identity(new_tokens.access_token)))
}

/// Get a manager and connected client for a running VM.
///
/// Returns an error if the VM is not running.
pub fn ensure_connected(name: &str) -> anyhow::Result<(AgentManager, AgentClient)> {
    tracing::info!(machine = name, "connecting to machine");
    let manager = get_manager(name)?;

    if manager.try_connect_existing().is_none() {
        anyhow::bail!("machine '{}' is not running. Use 'smol start' first.", name);
    }

    let socket = manager.vsock_socket();
    tracing::debug!(machine = name, socket = %socket.display(), "connecting to machine agent over vsock");
    let client = AgentClient::connect_with_retry(socket)?;
    tracing::debug!(machine = name, "connected to machine agent");
    Ok((manager, client))
}

#[cfg(test)]
mod retry_tests {
    use super::{is_retryable_status, retry_backoff};
    use reqwest::StatusCode;

    #[test]
    fn retryable_only_for_transient_infra_statuses() {
        for s in [429u16, 502, 503, 504] {
            assert!(
                is_retryable_status(StatusCode::from_u16(s).unwrap()),
                "{s} should retry"
            );
        }
        // Client errors and deterministic 500 are NOT retried.
        for s in [200u16, 400, 401, 403, 404, 409, 422, 500] {
            assert!(
                !is_retryable_status(StatusCode::from_u16(s).unwrap()),
                "{s} should not retry"
            );
        }
    }

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(retry_backoff(1).as_millis(), 200);
        assert_eq!(retry_backoff(2).as_millis(), 400);
        assert_eq!(retry_backoff(3).as_millis(), 800);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smolvm::registry::{RegistryConfig, RegistryEntry};

    #[test]
    fn test_build_registry_client_https_by_default() {
        let config = RegistryConfig::default();
        let client = build_registry_client(
            "registry.smolmachines.com",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.base_url(), "https://registry.smolmachines.com");
    }

    #[test]
    fn test_build_registry_client_http_for_localhost() {
        let config = RegistryConfig::default();
        let client = build_registry_client(
            "localhost:5050",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.base_url(), "http://localhost:5050");
    }

    #[test]
    fn test_build_registry_client_http_for_127() {
        let config = RegistryConfig::default();
        let client = build_registry_client(
            "127.0.0.1:5000",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.base_url(), "http://127.0.0.1:5000");
    }

    #[test]
    fn test_build_registry_client_uses_mirror() {
        let mut config = RegistryConfig::default();
        config.registries.insert(
            "registry.smolmachines.com".to_string(),
            RegistryEntry {
                username: None,
                password: None,
                password_env: None,
                mirror: Some("mirror.example.com".to_string()),
                ..Default::default()
            },
        );
        let client = build_registry_client(
            "registry.smolmachines.com",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.base_url(), "https://mirror.example.com");
    }

    #[test]
    fn test_build_registry_client_identity_token_on_non_smolmachines_registry() {
        // A self-hosted registry that uses the identity_token path must get
        // with_identity_token(), not with_token(), regardless of hostname.
        // Previously this was gated on the hostname == SMOLMACHINES_REGISTRY.
        let mut config = RegistryConfig::default();
        config.registries.insert(
            "registry.selfhosted.example.com".to_string(),
            RegistryEntry {
                identity_token: Some("eyJ_self_hosted_jwt".to_string()),
                ..Default::default()
            },
        );
        let client = build_registry_client(
            "registry.selfhosted.example.com",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(
            client.identity_token(),
            Some("eyJ_self_hosted_jwt"),
            "identity_token must be used for any registry that sets the field"
        );
    }

    #[test]
    fn test_build_registry_client_standard_creds_use_basic_auth() {
        // A real username (not "token") triggers the Docker/OCI Basic challenge path.
        let mut config = RegistryConfig::default();
        config.registries.insert(
            "ghcr.io".to_string(),
            RegistryEntry {
                username: Some("github_user".to_string()),
                password: Some("ghp_secret".to_string()),
                ..Default::default()
            },
        );
        let client = build_registry_client(
            "ghcr.io",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.identity_token(), None);
        assert_eq!(
            client.basic_credentials(),
            Some(("github_user", "ghp_secret")),
            "real username must route to with_basic_credentials()"
        );
    }

    #[test]
    fn test_build_registry_client_token_username_is_direct_bearer() {
        // username="token" is the legacy direct-bearer convention; password is the bearer value.
        let mut config = RegistryConfig::default();
        config.registries.insert(
            "custom.io".to_string(),
            RegistryEntry {
                username: Some("token".to_string()),
                password: Some("bearer_value".to_string()),
                ..Default::default()
            },
        );
        let client = build_registry_client(
            "custom.io",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(client.identity_token(), None);
        assert_eq!(client.basic_credentials(), None);
    }

    #[test]
    fn test_build_registry_client_docker_hub_uses_api_endpoint() {
        let config = RegistryConfig::default();
        let client = build_registry_client(
            "docker.io",
            &config,
            &smolvm::settings::CloudSection::default(),
        )
        .unwrap();
        assert_eq!(
            client.base_url(),
            "https://registry-1.docker.io",
            "docker.io must map to registry-1.docker.io"
        );
    }

    /// Document a load-bearing constraint: `build_registry_client` must NOT be
    /// called from inside an active tokio runtime when the identity token is
    /// expired AND a refresh_token is configured.
    ///
    /// The silent refresh path in `resolve_identity_token` uses
    /// `Runtime::new() + block_on` internally — and tokio panics if that
    /// pattern is invoked from a thread already driving another runtime
    /// ("Cannot start a runtime from within a runtime").
    ///
    /// Callers MUST resolve the registry client sync, **before** entering any
    /// tokio runtime. `pull.rs`, `push.rs`, and the Phase 1 in `deploy.rs::run`
    /// all follow this pattern.
    ///
    /// If this test stops panicking (e.g., `resolve_identity_token` was
    /// refactored to use `block_in_place` or made async), the constraint has
    /// been removed and this test can be deleted along with the Phase-1
    /// boilerplate it justifies.
    #[tokio::test]
    #[should_panic(expected = "Cannot start a runtime from within a runtime")]
    async fn build_registry_client_inside_runtime_panics_on_refresh() {
        let mut config = RegistryConfig::default();
        config.registries.insert(
            "registry.smolmachines.com".to_string(),
            RegistryEntry {
                identity_token: Some("expired_jwt".to_string()),
                refresh_token: Some("refresh_token_value".to_string()),
                // Past timestamp triggers the refresh path.
                expires_at: Some(1),
                ..Default::default()
            },
        );
        // Inside #[tokio::test] we're in an outer runtime. The inner
        // Runtime::new() + block_on inside resolve_identity_token will panic.
        // We don't reach any HTTP call — the panic fires during scheduler start.
        let _ = build_registry_client(
            "registry.smolmachines.com",
            &config,
            &smolvm::settings::CloudSection::default(),
        );
    }
}

/// Decode a JWT's payload segment (no signature check — we only read claims).
fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Normalize a tenant id to an OCI-safe namespace segment.
///
/// MUST match the registry token-service `sanitizeTenant` and smolfleet's
/// `normalize_tenant` exactly, so the CLI pushes to the same repo the control
/// plane references and the pull token is scoped to.
pub fn normalize_tenant(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Tenant namespace from the stored Auth0 identity token for `registry`
/// (`org_id`, else `sub`), or `None` when there's no identity token.
pub fn tenant_namespace(
    registry: &str,
    config: &smolvm::registry::RegistryConfig,
) -> Option<String> {
    let entry = config.registries.get(registry)?;
    let jwt = entry.identity_token.as_ref()?;

    // Authoritative: ask the control plane who we are. The registry token
    // service grants push on `tenants/<canonical-id>/...` where the canonical
    // id is minted server-side at provisioning — it CANNOT be derived from the
    // JWT (the old claim-based guess produced `google-oauth2-...`, which the
    // server rejects).
    if let Some(t) = me_tenant_namespace(registry, jwt) {
        return Some(t);
    }

    // Fallback (offline / older control planes that namespaced by normalized
    // external id): derive from JWT claims.
    let payload = decode_jwt_payload(jwt)?;
    let raw = payload
        .get("org_id")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("sub").and_then(|v| v.as_str()))?;
    let t = normalize_tenant(raw);
    (!t.is_empty()).then_some(t)
}

/// Resolve the canonical tenant namespace from the control plane's `GET /v1/me`
/// (`registryNamespace: "tenants/<id>"`). Only the smolmachines registry has a
/// known paired API; `SMOL_CLOUD_API` overrides the base for self-hosted
/// control planes. Returns the bare tenant id (without the `tenants/` prefix).
fn me_tenant_namespace(registry: &str, jwt: &str) -> Option<String> {
    let base = match std::env::var("SMOL_CLOUD_API") {
        Ok(v) if !v.is_empty() => v,
        _ if registry == smolvm::registry::SMOLMACHINES_REGISTRY => {
            smolvm::registry::SMOLMACHINES_API.to_string()
        }
        _ => return None,
    };
    let rt = tokio::runtime::Runtime::new().ok()?;
    rt.block_on(async {
        let resp = http_client()
            .ok()?
            .get(format!("{}/v1/me", base.trim_end_matches('/')))
            .bearer_auth(jwt)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::debug!(status = %resp.status(), "/v1/me lookup failed; falling back to JWT-derived namespace");
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let ns = v.get("registryNamespace")?.as_str()?;
        ns.strip_prefix("tenants/").map(str::to_string)
    })
}

/// Namespace a bare artifact repo under the caller's tenant — but only for the
/// smolmachines registry, and only when not already scoped (`tenants/`,
/// `library/`). Other registries and already-scoped repos pass through. Keeps a
/// `smol push <name>` against smolmachines inside the tenant's namespace, which
/// is all the registry token grants.
pub fn namespaced_repo(
    registry: &str,
    repo: &str,
    config: &smolvm::registry::RegistryConfig,
) -> String {
    if registry != smolvm::registry::SMOLMACHINES_REGISTRY {
        return repo.to_string();
    }
    if repo == "library" || repo.starts_with("library/") || repo.starts_with("tenants/") {
        return repo.to_string();
    }
    match tenant_namespace(registry, config) {
        Some(t) => format!("tenants/{t}/{repo}"),
        None => repo.to_string(),
    }
}

/// Resolve an artifact reference supplied either positionally (`smol pull
/// alpine:latest`) or via the legacy `--ref` flag. Commands declare both with
/// `conflicts_with`, so clap rejects passing both; this just requires that one
/// was given.
pub fn require_ref<'a>(
    positional: Option<&'a str>,
    flag: Option<&'a str>,
) -> anyhow::Result<&'a str> {
    positional
        .or(flag)
        .ok_or_else(|| anyhow::anyhow!("a reference is required, e.g. `smol pull alpine:latest`"))
}

#[cfg(test)]
mod namespace_tests {
    use super::{decode_jwt_payload, namespaced_repo, normalize_tenant};
    use smolvm::registry::SMOLMACHINES_REGISTRY;

    #[test]
    fn normalize_matches_cross_service_rule() {
        assert_eq!(normalize_tenant("Org_ACME"), "org_acme");
        assert_eq!(normalize_tenant("auth0|abc"), "auth0-abc");
        assert_eq!(normalize_tenant("a/b c"), "a-b-c");
    }

    #[test]
    fn decode_payload_reads_claims() {
        use base64::Engine;
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"org_id":"acme"}"#);
        let v = decode_jwt_payload(&format!("h.{p}.s")).unwrap();
        assert_eq!(v["org_id"], "acme");
    }

    #[test]
    fn tenant_namespace_falls_back_to_jwt_when_me_unreachable() {
        // JWT payload {"sub":"google-oauth2|12345"} — the legacy fallback path.
        let payload = base64_url(br#"{"sub":"google-oauth2|12345"}"#);
        let jwt = format!("h.{payload}.s");
        let mut cfg = smolvm::registry::RegistryConfig::default();
        cfg.set_identity_token(SMOLMACHINES_REGISTRY, &jwt);

        // Point /v1/me at a dead port so the authoritative lookup fails fast.
        std::env::set_var("SMOL_CLOUD_API", "http://127.0.0.1:9");
        let ns = super::tenant_namespace(SMOLMACHINES_REGISTRY, &cfg);
        std::env::remove_var("SMOL_CLOUD_API");

        assert_eq!(ns.as_deref(), Some("google-oauth2-12345"));
    }

    fn base64_url(data: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    #[test]
    fn namespaced_repo_only_touches_bare_smolmachines_repos() {
        let cfg = smolvm::registry::RegistryConfig::default();
        assert_eq!(namespaced_repo("docker.io", "app", &cfg), "app");
        assert_eq!(
            namespaced_repo(SMOLMACHINES_REGISTRY, "tenants/x/app", &cfg),
            "tenants/x/app"
        );
        assert_eq!(
            namespaced_repo(SMOLMACHINES_REGISTRY, "library/python", &cfg),
            "library/python"
        );
        assert_eq!(namespaced_repo(SMOLMACHINES_REGISTRY, "app", &cfg), "app");
    }
}

#[cfg(test)]
mod name_validation_tests {
    use super::validate_machine_name;

    #[test]
    fn rejects_traversal_separators_and_bad_chars() {
        for bad in [
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "a b",
            "x".repeat(64).as_str(),
        ] {
            assert!(validate_machine_name(bad).is_err(), "should reject {bad:?}");
        }
        assert!(validate_machine_name("").is_err());
    }

    #[test]
    fn accepts_safe_names() {
        for ok in ["default", "my-vm_1", "Web2", "a", "v1.2"] {
            assert!(validate_machine_name(ok).is_ok(), "should accept {ok:?}");
        }
    }
}
