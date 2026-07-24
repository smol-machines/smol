//! smol config — manage CLI configuration.

use clap::{Args, Subcommand};
use smolvm::settings::SmolSettings;

#[derive(Args, Debug)]
pub struct ConfigCmd {
    #[command(subcommand)]
    pub command: ConfigSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigSubcommand {
    /// Set a configuration value
    Set(ConfigSetCmd),
    /// Show current configuration
    Show,
}

#[derive(Args, Debug)]
pub struct ConfigSetCmd {
    /// Key to set (e.g., "cloud")
    pub key: String,
    /// Value to set (e.g., "http://localhost:9090")
    pub value: String,
}

impl ConfigCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ConfigSubcommand::Set(cmd) => cmd.run(),
            ConfigSubcommand::Show => show_config(),
        }
    }
}

impl ConfigSetCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.key.as_str() {
            "cloud" => {
                validate_cloud_endpoint(&self.value)?;
                let mut settings = SmolSettings::load()?;
                settings.cloud.endpoint = Some(self.value.clone());
                settings.save()?;
                eprintln!("Cloud endpoint set to: {}", self.value);
                Ok(())
            }
            "api_key" | "apikey" | "api-key" => {
                let mut settings = SmolSettings::load()?;
                settings.cloud.api_key = Some(self.value.clone());
                settings.save()?;
                eprintln!("API key configured.");
                Ok(())
            }
            other => anyhow::bail!("unknown config key: '{}'. Available: cloud, api_key", other),
        }
    }
}

/// Reject cloud endpoints that would send the bearer token in cleartext.
/// `https://` is required, except for loopback hosts (local dev against a
/// control plane on 127.0.0.1/localhost) where `http://` is allowed.
fn validate_cloud_endpoint(value: &str) -> anyhow::Result<()> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        // Extract the host. A bracketed IPv6 literal (`[::1]`) must be pulled out
        // whole: a plain split on ':' would shatter it, so the `[::1]`/`::1`
        // arms below would never match and IPv6 loopback dev endpoints would be
        // wrongly rejected. A malformed bracket (e.g. `[::1].evil.com`) falls
        // through to the raw string, which matches no allowed host and is refused.
        let host = if let Some(after) = rest.strip_prefix('[') {
            match after.split_once(']') {
                Some((inner, tail))
                    if tail.is_empty() || tail.starts_with(':') || tail.starts_with('/') =>
                {
                    format!("[{inner}]")
                }
                _ => rest.to_string(),
            }
        } else {
            rest.split(['/', ':']).next().unwrap_or("").to_string()
        };
        if host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1" {
            return Ok(());
        }
        anyhow::bail!(
            "refusing to set an http:// cloud endpoint to a non-loopback host \
             ('{value}') — the API token would be sent in cleartext. Use https://, \
             or http://localhost for local development."
        );
    }
    anyhow::bail!(
        "cloud endpoint must start with https:// (got '{value}'). Example: \
         https://api.smolfleet.example"
    )
}

fn show_config() -> anyhow::Result<()> {
    let settings = SmolSettings::load()?;
    println!("cloud.endpoint = {}", settings.cloud.endpoint.as_deref().unwrap_or("(not set)"));
    println!("cloud.api_key  = {}", mask_secret(settings.cloud.api_key.as_deref()));

    // Registries are the other half of the config; enumerate both sections so
    // `config show` is a complete picture of config.toml. Credentials are
    // described by type, never printed.
    print_registries("machines", &settings.machines);
    print_registries("images", &settings.images);
    Ok(())
}

/// Mask a secret for display: keep enough to recognize it, hide the rest.
fn mask_secret(value: Option<&str>) -> String {
    match value {
        Some(k) if k.len() > 12 => format!("{}...{}", &k[..8], &k[k.len() - 4..]),
        Some(_) => "(set)".to_string(),
        None => "(not set)".to_string(),
    }
}

/// Print a registry section's entries with credentials described, not revealed.
fn print_registries(section: &str, config: &smolvm::registry::RegistryConfig) {
    if config.registries.is_empty() {
        return;
    }
    let mut hosts: Vec<&String> = config.registries.keys().collect();
    hosts.sort();
    for host in hosts {
        let entry = &config.registries[host];
        let auth = if entry.identity_token.is_some() {
            "identity-token".to_string()
        } else if let Some(var) = &entry.password_env {
            format!("password_env:{var}")
        } else if entry.password.is_some() {
            "password".to_string()
        } else {
            "none".to_string()
        };
        println!("{section}.registries.\"{host}\" = {auth}");
    }
}

#[cfg(test)]
mod tests {
    use super::validate_cloud_endpoint;

    #[test]
    fn https_endpoints_are_accepted() {
        assert!(validate_cloud_endpoint("https://api.example.com").is_ok());
        assert!(validate_cloud_endpoint("HTTPS://API.EXAMPLE.COM").is_ok());
    }

    #[test]
    fn http_loopback_is_allowed_for_dev() {
        assert!(validate_cloud_endpoint("http://localhost:9090").is_ok());
        assert!(validate_cloud_endpoint("http://127.0.0.1:9090").is_ok());
    }

    #[test]
    fn http_to_public_host_is_rejected() {
        assert!(validate_cloud_endpoint("http://evil.example.com").is_err());
        assert!(validate_cloud_endpoint("http://10.0.0.5:9090").is_err());
    }

    #[test]
    fn missing_or_unknown_scheme_is_rejected() {
        assert!(validate_cloud_endpoint("api.example.com").is_err());
        assert!(validate_cloud_endpoint("ftp://example.com").is_err());
    }

    #[test]
    fn http_ipv6_loopback_is_allowed_for_dev() {
        // Bracketed IPv6 loopback must be accepted (was wrongly rejected when the
        // host was extracted with a plain ':' split).
        assert!(validate_cloud_endpoint("http://[::1]").is_ok());
        assert!(validate_cloud_endpoint("http://[::1]:9090").is_ok());
        assert!(validate_cloud_endpoint("http://[::1]/path").is_ok());
    }

    #[test]
    fn http_ipv6_bracket_bypasses_are_rejected() {
        // Non-loopback IPv6 and bracket-lookalikes must not be treated as loopback.
        assert!(validate_cloud_endpoint("http://[dead::beef]").is_err());
        assert!(validate_cloud_endpoint("http://[dead::beef]:9090").is_err());
        assert!(validate_cloud_endpoint("http://[::1].evil.com").is_err());
        assert!(validate_cloud_endpoint("http://[::1]@evil.com").is_err());
    }
}
