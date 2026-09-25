//! Choosing where a machine runs.

use smol_cloud::blocking::Client;

use crate::config::EgressInterceptor;
use crate::error::Result;

/// Where a machine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// The embedded engine, in this process.
    Local,
    /// smol cloud, over the control plane's REST API.
    Cloud,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
        })
    }
}

/// Which target to use, and how to reach it.
///
/// The default is local, and only an *explicit* credential moves that: an
/// [`api_key`](ConnectOptions::api_key) or `SMOL_CLOUD_TOKEN` is a deliberate
/// act by the caller, whereas a CLI session on disk is a side effect of
/// `smol auth login`. Letting a stored session decide would send every default
/// `Machine::create` on a machine that had ever logged in to the cloud, and
/// break outright once that key expired.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Local interceptor binding required to start a stopped protected machine.
    pub egress_interceptor: Option<EgressInterceptor>,
    /// Force a target. `None` decides from the credential.
    pub target: Option<Target>,
    /// Cloud base URL. Falls back to `SMOL_CLOUD_URL`, then the CLI session's
    /// endpoint, then the public cloud.
    pub base_url: Option<String>,
    /// Cloud API key, `smk_…`.
    pub api_key: Option<String>,
}

impl ConnectOptions {
    /// Decide from the credential: cloud when one is present, local otherwise.
    pub fn new() -> Self {
        Self::default()
    }

    /// Always use the embedded engine, whatever credentials exist.
    pub fn local() -> Self {
        Self {
            target: Some(Target::Local),
            ..Self::default()
        }
    }

    /// Always use smol cloud, taking the credential from the environment or
    /// the CLI session.
    pub fn cloud() -> Self {
        Self {
            target: Some(Target::Cloud),
            ..Self::default()
        }
    }

    /// Use smol cloud with this key.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            target: Some(Target::Cloud),
            api_key: Some(api_key.into()),
            ..Self::default()
        }
    }

    /// Talk to a cloud at this base URL.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Use this API key.
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Bind a trusted host egress service when reconnecting to a local machine.
    pub fn egress_interceptor(mut self, binding: EgressInterceptor) -> Self {
        self.egress_interceptor = Some(binding);
        self
    }

    /// The credential the caller named outright, environment included.
    fn explicit_key(&self) -> Option<String> {
        self.api_key.clone().or_else(|| {
            std::env::var(smol_cloud::credentials::TOKEN_ENV)
                .ok()
                .filter(|key| !key.is_empty())
        })
    }

    /// Which target this request selects.
    pub fn target(&self) -> Target {
        match self.target {
            Some(target) => target,
            None if self.explicit_key().is_some() => Target::Cloud,
            None => Target::Local,
        }
    }

    /// Build a cloud client, or say what is missing.
    pub(crate) fn client(&self) -> Result<Client> {
        Ok(Client::resolve(
            self.base_url.clone(),
            self.api_key.clone(),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_target_is_local_unless_a_key_says_otherwise() {
        assert_eq!(ConnectOptions::new().target(), Target::Local);
        assert_eq!(
            ConnectOptions::new().api_key("smk_x").target(),
            Target::Cloud
        );
        // An explicit local target outranks a credential.
        assert_eq!(
            ConnectOptions::local().api_key("smk_x").target(),
            Target::Local
        );
        assert_eq!(ConnectOptions::cloud().target(), Target::Cloud);
    }

    #[test]
    fn asking_for_the_cloud_with_a_key_needs_nothing_else() {
        let client = ConnectOptions::with_api_key("smk_test")
            .base_url("https://cloud.test")
            .client()
            .expect("an explicit key and URL resolve without any environment");
        assert_eq!(client.credentials().base_url(), "https://cloud.test");
    }
}
