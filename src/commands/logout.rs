//! smol logout — remove stored credentials for a registry.

use clap::Args;
use smolvm::settings::SmolSettings;

#[derive(Args, Debug)]
pub struct LogoutCmd {
    /// Registry hostname (default: registry.smolmachines.com)
    #[arg(long, value_name = "REGISTRY")]
    pub registry: Option<String>,
}

impl LogoutCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let registry = self
            .registry
            .unwrap_or_else(|| smolvm::registry::SMOLMACHINES_REGISTRY.to_string());

        let mut settings = SmolSettings::load().unwrap_or_default();

        if !revoke(&mut settings, &registry) {
            eprintln!("Not logged in to {} (no credentials found)", registry);
            return Ok(());
        }

        settings.save()?;

        eprintln!("Logged out of {}", registry);
        Ok(())
    }
}

/// Remove all stored credentials for `registry`. Returns `true` if anything
/// was actually cleared.
///
/// The smolmachines registry also seeds a parallel `settings.cloud` section
/// that every cloud command authenticates from. Dropping the registry entry
/// alone leaves that session (and its refresh token) live, so revoke it here
/// too — even when the registry entry was already absent, since the two stores
/// can drift out of sync.
fn revoke(settings: &mut SmolSettings, registry: &str) -> bool {
    let had_registry = settings.machines.registries.remove(registry).is_some();

    let mut cleared_cloud = false;
    if registry == smolvm::registry::SMOLMACHINES_REGISTRY {
        cleared_cloud = settings.cloud.api_key.is_some()
            || settings.cloud.refresh_token.is_some()
            || settings.cloud.token_expires_at.is_some();
        settings.cloud.api_key = None;
        settings.cloud.refresh_token = None;
        settings.cloud.token_expires_at = None;
    }

    had_registry || cleared_cloud
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged_in_to_cloud() -> SmolSettings {
        let mut settings = SmolSettings::default();
        let registry = smolvm::registry::SMOLMACHINES_REGISTRY;
        settings.machines.set_identity_token(registry, "access-token");
        settings.cloud.api_key = Some("access-token".to_string());
        settings.cloud.refresh_token = Some("refresh-token".to_string());
        settings.cloud.token_expires_at = Some(1700000000);
        settings
    }

    #[test]
    fn logout_clears_parallel_cloud_session() {
        let mut settings = logged_in_to_cloud();
        assert!(revoke(&mut settings, smolvm::registry::SMOLMACHINES_REGISTRY));
        // Both stores must be empty — a lingering refresh token is the bug.
        assert!(!settings
            .machines
            .registries
            .contains_key(smolvm::registry::SMOLMACHINES_REGISTRY));
        assert!(settings.cloud.api_key.is_none());
        assert!(settings.cloud.refresh_token.is_none());
        assert!(settings.cloud.token_expires_at.is_none());
    }

    #[test]
    fn logout_clears_cloud_even_when_registry_entry_absent() {
        // Stores drifted: registry map empty but cloud session still live.
        let mut settings = SmolSettings::default();
        settings.cloud.api_key = Some("access-token".to_string());
        settings.cloud.refresh_token = Some("refresh-token".to_string());
        assert!(revoke(&mut settings, smolvm::registry::SMOLMACHINES_REGISTRY));
        assert!(settings.cloud.api_key.is_none());
        assert!(settings.cloud.refresh_token.is_none());
    }

    #[test]
    fn logout_of_other_registry_leaves_cloud_session_intact() {
        let mut settings = logged_in_to_cloud();
        settings.machines.set_token("ghcr.io", "ghcr-token");
        assert!(revoke(&mut settings, "ghcr.io"));
        // The cloud session belongs to smolmachines, not GHCR.
        assert_eq!(settings.cloud.api_key.as_deref(), Some("access-token"));
        assert_eq!(settings.cloud.refresh_token.as_deref(), Some("refresh-token"));
    }

    #[test]
    fn logout_when_not_logged_in_reports_nothing_cleared() {
        let mut settings = SmolSettings::default();
        assert!(!revoke(&mut settings, smolvm::registry::SMOLMACHINES_REGISTRY));
        assert!(!revoke(&mut settings, "ghcr.io"));
    }
}
