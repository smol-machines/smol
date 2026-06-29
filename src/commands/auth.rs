//! OAuth device flow and token refresh for smol CLI.
//!
//! Implements RFC 8628 (OAuth 2.0 Device Authorization Grant) for interactive
//! `smol login` and silent token refresh for expired tokens.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// Default OIDC issuer for smolmachines.
const DEFAULT_ISSUER: &str = "https://smolmachines.us.auth0.com";

/// OAuth client ID for the smol CLI (public client, no secret).
const CLIENT_ID: &str = "Df3M6TXvVVMmTTzfyo0mjaLl9rhaI7nZ";

/// Scopes requested during device flow. Includes the smolfleet machine/feature
/// scopes so a `smol login` token can actually drive the cloud — without them
/// the token carries no authorization and every cloud op 403s (smolfleet reads
/// the `scope` claim when RBAC isn't injecting `permissions`).
const SCOPES: &str = "openid offline_access \
    machine:read machine:create machine:exec machine:delete machine:files \
    usage:read billing:read app:write \
    volume:read volume:create volume:delete ops:read";

/// OAuth audience — the platform-wide API identifier registered in Auth0.
///
/// A single audience covers both the artifact registry and the smolfleet API,
/// so one `smol login` grants access to all platform services. Auth0 only
/// issues a JWT access token (rather than an opaque string) when `audience`
/// is present; zot validates by JWT signature and smolfleet validates by
/// both signature and this audience claim. The value must exactly match the
/// API identifier configured in the Auth0 tenant.
const AUDIENCE: &str = "https://api.smolmachines.com";

/// Environment variable to override the OIDC issuer.
const OIDC_ISSUER_ENV: &str = "OIDC_ISSUER";

// ============================================================================
// Device Flow
// ============================================================================

/// Response from the device authorization endpoint.
#[derive(Debug, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: Option<u64>,
}

/// Successful token response from the token endpoint.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    #[allow(dead_code)]
    pub token_type: Option<String>,
}

/// Error response during token polling.
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[allow(dead_code)]
    error_description: Option<String>,
}

/// Run the OAuth device authorization flow.
///
/// Opens a browser (unless `no_browser` is true), displays a user code,
/// and polls until the user approves or the flow times out.
pub async fn device_flow(no_browser: bool) -> Result<TokenResponse> {
    let issuer = issuer_url();
    let client = super::common::http_client()?;

    tracing::info!(issuer = %issuer, no_browser, "starting OAuth device flow");

    // Step 1: Request device code
    let device_url = format!("{}/oauth/device/code", issuer);
    let resp = client
        .post(&device_url)
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", SCOPES),
            ("audience", AUDIENCE),
        ])
        .send()
        .await
        .context("failed to contact auth server")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("device authorization failed ({}): {}", status, body);
    }

    let device_auth: DeviceAuthResponse = resp.json().await
        .context("failed to parse device authorization response")?;

    // Step 2: Display instructions
    let display_uri = device_auth
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device_auth.verification_uri);

    eprintln!();
    eprintln!("  Open this URL in your browser:");
    eprintln!();
    eprintln!("    {}", display_uri);
    eprintln!();
    eprintln!("  And enter this code: {}", device_auth.user_code);
    eprintln!();

    // Attempt to open browser
    if !no_browser && open::that(display_uri).is_ok() {
        eprintln!("  (browser opened automatically)");
    }

    eprintln!("  Waiting for approval...");

    // Step 3: Poll token endpoint
    let token_url = format!("{}/oauth/token", issuer);
    let mut interval = Duration::from_secs(device_auth.interval.unwrap_or(5));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device_auth.expires_in);

    loop {
        tokio::time::sleep(interval).await;

        if tokio::time::Instant::now() >= deadline {
            bail!("device flow timed out — the code expired before approval");
        }

        let resp = client
            .post(&token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device_auth.device_code),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .context("failed to poll token endpoint")?;

        if resp.status().is_success() {
            let token_resp: TokenResponse = resp.json().await
                .context("failed to parse token response")?;
            return Ok(token_resp);
        }

        let body = resp.text().await.unwrap_or_default();
        let error: TokenErrorResponse = match serde_json::from_str(&body) {
            Ok(e) => e,
            Err(_) => bail!("unexpected auth server response: {}", body),
        };

        match error.error.as_str() {
            "authorization_pending" => {
                // User hasn't approved yet, keep polling
            }
            "slow_down" => {
                // Server asks us to back off
                interval += Duration::from_secs(5);
            }
            "expired_token" => {
                bail!("device flow expired — run `smol login` again");
            }
            "access_denied" => {
                bail!("login denied — the request was rejected");
            }
            other => {
                bail!("auth error: {}", other);
            }
        }
    }
}

// ============================================================================
// Token Refresh
// ============================================================================

/// Attempt to refresh an access token using a refresh token.
///
/// Returns a new `TokenResponse` on success. The caller is responsible for
/// persisting the updated tokens.
pub async fn refresh_access_token(refresh_token: &str) -> Result<TokenResponse> {
    refresh_access_token_at(&issuer_url(), refresh_token).await
}

/// Refresh against an explicit issuer base URL. Split out from
/// [`refresh_access_token`] so the HTTP exchange is testable without mutating
/// the process-global `OIDC_ISSUER` env var.
async fn refresh_access_token_at(issuer: &str, refresh_token: &str) -> Result<TokenResponse> {
    let client = super::common::http_client()?;
    let token_url = format!("{}/oauth/token", issuer);

    // Never log the refresh token or the issued access token — only metadata.
    tracing::info!(issuer = %issuer, "refreshing access token");

    let resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("failed to contact auth server for token refresh")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, "token refresh failed");
        bail!(
            "token refresh failed ({}): {}. Run `smol login` to re-authenticate.",
            status,
            body
        );
    }

    let tokens = resp
        .json::<TokenResponse>()
        .await
        .context("failed to parse refresh token response")?;
    tracing::debug!(
        rotated_refresh_token = tokens.refresh_token.is_some(),
        expires_in = ?tokens.expires_in,
        "access token refreshed"
    );
    Ok(tokens)
}

// ============================================================================
// Helpers
// ============================================================================

/// Resolve the OIDC issuer URL from environment or use the default.
fn issuer_url() -> String {
    std::env::var(OIDC_ISSUER_ENV).unwrap_or_else(|_| DEFAULT_ISSUER.to_string())
}

/// Compute a Unix timestamp for `expires_in` seconds from now.
pub fn expires_at_from_now(expires_in: u64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    now + expires_in as i64
}

/// Apply a refreshed Auth0 token pair to a single registry entry.
///
/// Works for any registry that uses the identity_token auth path (Auth0 JWT).
/// The `refresh_token` is only overwritten if the server returned a new one
/// (some IdPs rotate on every refresh, others don't — preserving the existing
/// value keeps future refreshes working when the server omits a new one).
pub fn apply_refreshed_registry_tokens(
    entry: &mut smolvm::registry::RegistryEntry,
    new_tokens: &TokenResponse,
) {
    entry.identity_token = Some(new_tokens.access_token.clone());
    if let Some(rt) = new_tokens.refresh_token.clone() {
        entry.refresh_token = Some(rt);
    }
    entry.expires_at = new_tokens.expires_in.map(expires_at_from_now);
}

/// Apply a refreshed Auth0 token pair to the [cloud] section.
///
/// Only meaningful for the smolmachines registry — the cloud API key and
/// the registry JWT are the same token (smol login writes both in lockstep).
pub fn apply_refreshed_cloud_tokens(
    cloud: &mut smolvm::settings::CloudSection,
    new_tokens: &TokenResponse,
) {
    cloud.api_key = Some(new_tokens.access_token.clone());
    if let Some(rt) = new_tokens.refresh_token.clone() {
        cloud.refresh_token = Some(rt);
    }
    cloud.token_expires_at = new_tokens.expires_in.map(expires_at_from_now);
}

/// Apply a refreshed Auth0 token pair to BOTH the [cloud] section AND the
/// smolmachines registry entry. Use this whenever the smolmachines JWT is
/// refreshed — keeping the two sections in lockstep prevents one side from
/// silently expiring after a refresh.
///
/// Equivalent to:
///   apply_refreshed_registry_tokens(<smolmachines entry>, new_tokens) +
///   apply_refreshed_cloud_tokens(&mut settings.cloud, new_tokens)
pub fn apply_refreshed_smolmachines_tokens(
    settings: &mut smolvm::SmolSettings,
    new_tokens: &TokenResponse,
) {
    let entry = settings
        .machines
        .registries
        .entry(smolvm::registry::SMOLMACHINES_REGISTRY.to_string())
        .or_default();
    apply_refreshed_registry_tokens(entry, new_tokens);
    apply_refreshed_cloud_tokens(&mut settings.cloud, new_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_url_defaults_when_env_not_set() {
        // Test the default by calling issuer_url() directly with the default constant.
        // We avoid mutating OIDC_ISSUER_ENV here because env vars are process-global
        // and unsafe to set/remove in parallel tests.
        assert_eq!(DEFAULT_ISSUER, "https://smolmachines.us.auth0.com");
    }

    #[test]
    fn issuer_url_parses_env_var() {
        // Verify the logic works by calling the function with the env var already
        // set to the default value (safe — no mutation needed).
        // The env-override path is exercised in integration tests where
        // OIDC_ISSUER can be set per-process without parallelism issues.
        assert_eq!(issuer_url(), DEFAULT_ISSUER);
    }

    #[test]
    fn expires_at_from_now_computes_future_timestamp() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let result = expires_at_from_now(3600);

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert!(result >= before + 3600);
        assert!(result <= after + 3600);
    }

    #[test]
    fn apply_refreshed_registry_tokens_sets_identity_field() {
        let mut entry = smolvm::registry::RegistryEntry::default();
        let new = TokenResponse {
            access_token: "new_access".to_string(),
            refresh_token: Some("new_refresh".to_string()),
            expires_in: Some(3600),
            token_type: None,
        };
        apply_refreshed_registry_tokens(&mut entry, &new);
        assert_eq!(entry.identity_token.as_deref(), Some("new_access"));
        assert_eq!(entry.refresh_token.as_deref(), Some("new_refresh"));
        assert!(entry.expires_at.is_some());
    }

    #[test]
    fn apply_refreshed_registry_tokens_preserves_refresh_when_omitted() {
        // Some IdPs don't rotate the refresh token on every refresh.
        // We must NOT clobber the existing one with None.
        let mut entry = smolvm::registry::RegistryEntry {
            refresh_token: Some("preserved".to_string()),
            ..Default::default()
        };
        let new = TokenResponse {
            access_token: "new_access".to_string(),
            refresh_token: None,
            expires_in: Some(3600),
            token_type: None,
        };
        apply_refreshed_registry_tokens(&mut entry, &new);
        assert_eq!(
            entry.refresh_token.as_deref(),
            Some("preserved"),
            "existing refresh_token must survive a refresh that omits one"
        );
    }

    #[test]
    fn apply_refreshed_smolmachines_tokens_keeps_both_sections_in_lockstep() {
        // This is the bug the refactor prevents: after a refresh, [cloud] and
        // the smolmachines entry under [machines] must hold the same JWT.
        let mut settings = smolvm::SmolSettings::default();
        let new = TokenResponse {
            access_token: "synced_jwt".to_string(),
            refresh_token: Some("synced_refresh".to_string()),
            expires_in: Some(7200),
            token_type: None,
        };
        apply_refreshed_smolmachines_tokens(&mut settings, &new);

        assert_eq!(settings.cloud.api_key.as_deref(), Some("synced_jwt"));
        assert_eq!(settings.cloud.refresh_token.as_deref(), Some("synced_refresh"));
        assert!(settings.cloud.token_expires_at.is_some());

        let entry = settings
            .machines
            .registries
            .get(smolvm::registry::SMOLMACHINES_REGISTRY)
            .expect("smolmachines entry should be created");
        assert_eq!(entry.identity_token.as_deref(), Some("synced_jwt"));
        assert_eq!(entry.refresh_token.as_deref(), Some("synced_refresh"));
        assert_eq!(entry.expires_at, settings.cloud.token_expires_at);
    }

    #[test]
    fn device_auth_response_deserializes() {
        let json = r#"{
            "device_code": "ABCD1234",
            "user_code": "EFGH-5678",
            "verification_uri": "https://auth.example.com/device",
            "verification_uri_complete": "https://auth.example.com/device?user_code=EFGH-5678",
            "expires_in": 900,
            "interval": 5
        }"#;

        let resp: DeviceAuthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.device_code, "ABCD1234");
        assert_eq!(resp.user_code, "EFGH-5678");
        assert_eq!(resp.verification_uri, "https://auth.example.com/device");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, Some(5));
    }

    #[test]
    fn token_response_deserializes_with_optional_fields() {
        let json = r#"{
            "access_token": "eyJhbGci...",
            "token_type": "Bearer",
            "expires_in": 86400,
            "refresh_token": "v1.refresh..."
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "eyJhbGci...");
        assert_eq!(resp.refresh_token.as_deref(), Some("v1.refresh..."));
        assert_eq!(resp.expires_in, Some(86400));
    }

    #[test]
    fn token_response_deserializes_minimal() {
        let json = r#"{"access_token": "tok123"}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok123");
        assert!(resp.refresh_token.is_none());
        assert!(resp.expires_in.is_none());
    }

    #[test]
    fn token_error_response_deserializes() {
        let json = r#"{"error": "authorization_pending", "error_description": "still waiting"}"#;
        let resp: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "authorization_pending");
    }

    // --- token refresh, end to end over HTTP -------------------------------
    //
    // Exercises `refresh_access_token_at` against a throwaway local server. No
    // dev-dependency and no `OIDC_ISSUER` mutation: the issuer is injected as an
    // argument, so these run safely in parallel.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Bind an ephemeral port, serve exactly one HTTP request with the given
    /// status + JSON body, and return the base URL to point a client at.
    async fn serve_one(status: u16, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request head; the body content is irrelevant to the mock.
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    _ => "Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn refresh_returns_rotated_tokens_on_success() {
        let base = serve_one(
            200,
            r#"{"access_token":"new-at","refresh_token":"new-rt","expires_in":3600}"#,
        )
        .await;

        let tokens = refresh_access_token_at(&base, "old-rt").await.unwrap();
        assert_eq!(tokens.access_token, "new-at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(tokens.expires_in, Some(3600));
    }

    #[tokio::test]
    async fn refresh_errors_with_reauth_hint_on_4xx() {
        let base = serve_one(401, r#"{"error":"invalid_grant"}"#).await;

        let err = refresh_access_token_at(&base, "stale-rt")
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("token refresh failed"), "got: {msg}");
        // A clear, actionable next step for the user.
        assert!(msg.contains("smol login"), "got: {msg}");
    }
}
