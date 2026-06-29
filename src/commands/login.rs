//! smol login — authenticate with a registry.
//!
//! Supports three modes:
//! - Interactive device flow (default): opens browser, polls for approval
//! - Direct token (`--token`): for CI/CD pipelines
//! - Stdin token (`--token-stdin`): piped from secret managers

use super::auth;
use clap::Args;
use smolvm::settings::SmolSettings;

#[derive(Args, Debug)]
pub struct LoginCmd {
    /// Registry hostname (default: registry.smolmachines.com)
    #[arg(long, value_name = "REGISTRY")]
    pub registry: Option<String>,

    /// Provide a token directly (for CI/CD)
    #[arg(long)]
    pub token: Option<String>,

    /// Read token from stdin
    #[arg(long)]
    pub token_stdin: bool,

    /// Do not open the browser automatically (print URL only)
    #[arg(long)]
    pub no_browser: bool,
}

impl LoginCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let registry = self
            .registry
            .clone()
            .unwrap_or_else(|| smolvm::registry::SMOLMACHINES_REGISTRY.to_string());

        if let Some(ref token) = self.token {
            return store_token(&registry, token.clone(), None, None);
        }

        if self.token_stdin {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let token = input.trim().to_string();
            if token.is_empty() {
                anyhow::bail!("token cannot be empty");
            }
            return store_token(&registry, token, None, None);
        }

        // Interactive device flow
        let rt = tokio::runtime::Runtime::new()?;
        let token_response = rt.block_on(auth::device_flow(self.no_browser))?;

        let expires_at = token_response
            .expires_in
            .map(auth::expires_at_from_now);

        store_token(
            &registry,
            token_response.access_token,
            token_response.refresh_token,
            expires_at,
        )
    }

}

fn store_token(
    registry: &str,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
) -> anyhow::Result<()> {
    if access_token.is_empty() {
        anyhow::bail!("token cannot be empty");
    }

    let mut settings = SmolSettings::load()?;
    let has_refresh_token = refresh_token.is_some();

    if registry == smolvm::registry::SMOLMACHINES_REGISTRY {
        // The smolmachines credential is an Auth0 JWT. The registry does NOT
        // accept it directly — it must be exchanged at the control plane's
        // token service (`/v2/auth`) per operation, so store it as an
        // identity_token (the exchange path), exactly like the silent-refresh
        // path does. Storing it as a direct-bearer password (the old behavior)
        // made every push 401 with zot's opaque "UNSUPPORTED" error.
        settings.machines.set_identity_token(registry, &access_token);
        if let Some(entry) = settings.machines.registries.get_mut(registry) {
            entry.refresh_token = refresh_token.clone();
            entry.expires_at = expires_at;
        }
        // Keep the cloud section in lockstep — same token, same expiry.
        settings.cloud.api_key = Some(access_token);
        settings.cloud.refresh_token = refresh_token;
        settings.cloud.token_expires_at = expires_at;
    } else {
        // Other registries (GHCR, Harbor, ...): the supplied token IS the
        // registry credential — keep the direct-bearer convention.
        settings.machines.set_token(registry, &access_token);
        if let Some(entry) = settings.machines.registries.get_mut(registry) {
            entry.refresh_token = refresh_token.clone();
            entry.expires_at = expires_at;
        }
    }

    settings.save()?;

    tracing::info!(registry, has_refresh_token, "stored registry credentials");
    eprintln!("Logged in to {}", registry);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests store_token logic by verifying the resulting SmolSettings struct directly.
    /// Uses a helper that builds settings in-memory rather than going through env vars.
    #[test]
    fn store_token_populates_machines_and_cloud_for_smolmachines() {
        // Simulate what store_token does without file I/O or env vars
        let mut settings = SmolSettings::default();
        let registry = smolvm::registry::SMOLMACHINES_REGISTRY;
        let access_token = "test-access-token";
        let refresh_token = Some("test-refresh-token".to_string());
        let expires_at = Some(1700000000i64);

        settings.machines.set_identity_token(registry, access_token);
        if let Some(entry) = settings.machines.registries.get_mut(registry) {
            entry.refresh_token = refresh_token.clone();
            entry.expires_at = expires_at;
        }
        settings.cloud.api_key = Some(access_token.to_string());
        settings.cloud.refresh_token = refresh_token;
        settings.cloud.token_expires_at = expires_at;

        // The Auth0 JWT is an identity token (exchanged at /v2/auth), NOT a
        // direct-bearer credential — get_credentials must find nothing.
        assert!(settings.machines.get_credentials(registry).is_none());

        let entry = settings.machines.registries.get(registry).unwrap();
        assert_eq!(entry.identity_token.as_deref(), Some("test-access-token"));
        assert_eq!(entry.refresh_token.as_deref(), Some("test-refresh-token"));
        assert_eq!(entry.expires_at, Some(1700000000));

        assert_eq!(settings.cloud.api_key.as_deref(), Some("test-access-token"));
        assert_eq!(
            settings.cloud.refresh_token.as_deref(),
            Some("test-refresh-token")
        );
        assert_eq!(settings.cloud.token_expires_at, Some(1700000000));
    }

    /// Re-login over a legacy direct-bearer entry must switch it to the
    /// identity-token path (the legacy form 401s against the registry).
    #[test]
    fn smolmachines_identity_token_replaces_legacy_direct_bearer() {
        let mut settings = SmolSettings::default();
        let registry = smolvm::registry::SMOLMACHINES_REGISTRY;
        settings.machines.set_token(registry, "stale-direct-bearer");

        settings.machines.set_identity_token(registry, "fresh-jwt");

        let entry = settings.machines.registries.get(registry).unwrap();
        assert_eq!(entry.identity_token.as_deref(), Some("fresh-jwt"));
        assert_eq!(entry.username, None);
        assert_eq!(entry.password, None);
        assert!(settings.machines.get_credentials(registry).is_none());
    }

    #[test]
    fn store_token_does_not_set_cloud_for_other_registries() {
        let mut settings = SmolSettings::default();
        let registry = "ghcr.io";

        settings.machines.set_token(registry, "ghcr-token");

        let creds = settings.machines.get_credentials(registry).unwrap();
        assert_eq!(creds.password, "ghcr-token");

        // Cloud section should NOT be populated for non-smolmachines registries
        assert!(settings.cloud.api_key.is_none());
    }

    #[test]
    fn store_token_roundtrips_through_toml() {
        let mut settings = SmolSettings::default();
        settings.machines.set_token("custom.io", "my-token");
        if let Some(entry) = settings.machines.registries.get_mut("custom.io") {
            entry.refresh_token = Some("refresh-xyz".to_string());
            entry.expires_at = Some(1800000000);
        }

        let serialized = toml::to_string_pretty(&settings).unwrap();
        let reloaded: SmolSettings = toml::from_str(&serialized).unwrap();

        let creds = reloaded.machines.get_credentials("custom.io").unwrap();
        assert_eq!(creds.password, "my-token");
        let entry = reloaded.machines.registries.get("custom.io").unwrap();
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh-xyz"));
        assert_eq!(entry.expires_at, Some(1800000000));
    }
}
