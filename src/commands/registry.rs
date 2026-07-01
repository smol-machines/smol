//! smol registry — the registry noun: what you're logged into, and what's in it.
//!
//! `ls` reports the registries you hold credentials for (both the `[machines]`
//! artifact registries and the `[images]` base-image registries) with secrets
//! described, never printed. `catalog` and `tags` browse a registry's contents
//! over the standard OCI distribution endpoints. `login`/`logout` are the
//! registry-scoped aliases of `smol auth login`/`logout`.

use clap::{Args, Subcommand};
use smolvm::registry::{RegistryConfig, SMOLMACHINES_REGISTRY};
use smolvm::settings::SmolSettings;

#[derive(Args, Debug)]
pub struct RegistryCmd {
    #[command(subcommand)]
    pub command: RegistrySubcommand,
}

#[derive(Subcommand, Debug)]
pub enum RegistrySubcommand {
    /// List the registries you have credentials for
    Ls(LsArgs),
    /// List repositories in a registry (`GET /v2/_catalog`)
    Catalog(CatalogArgs),
    /// List the tags of a repository
    Tags(TagsArgs),
    /// Authenticate to a registry (alias of `smol auth login`)
    Login(crate::commands::login::LoginCmd),
    /// Remove stored credentials for a registry (alias of `smol auth logout`)
    Logout(crate::commands::logout::LogoutCmd),
}

impl RegistryCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            RegistrySubcommand::Ls(cmd) => cmd.run(),
            RegistrySubcommand::Catalog(cmd) => cmd.run(),
            RegistrySubcommand::Tags(cmd) => cmd.run(),
            RegistrySubcommand::Login(cmd) => cmd.run(),
            RegistrySubcommand::Logout(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// registry ls — configured registries (credentials described, never printed)
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(serde::Serialize)]
struct RegistryRow {
    host: String,
    /// Which config section the entry lives in: "machines" or "images".
    scope: &'static str,
    /// How this registry authenticates, with no secret material.
    auth: String,
}

impl LsArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let settings = SmolSettings::load()?;
        let mut rows: Vec<RegistryRow> = Vec::new();
        collect_rows(&settings.machines, "machines", &mut rows);
        collect_rows(&settings.images, "images", &mut rows);
        rows.sort_by(|a, b| (a.host.as_str(), a.scope).cmp(&(b.host.as_str(), b.scope)));

        if self.json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }

        if rows.is_empty() {
            println!("No registries configured. Log in with: smol registry login");
            return Ok(());
        }

        let host_w = rows.iter().map(|r| r.host.len()).max().unwrap_or(4).max(4);
        println!("{:<host_w$}  {:<8}  AUTH", "HOST", "SCOPE");
        for r in &rows {
            println!("{:<host_w$}  {:<8}  {}", r.host, r.scope, r.auth);
        }
        Ok(())
    }
}

/// Describe how an entry authenticates without revealing any secret value.
fn collect_rows(config: &RegistryConfig, scope: &'static str, out: &mut Vec<RegistryRow>) {
    for (host, entry) in &config.registries {
        let auth = if entry.identity_token.is_some() {
            "identity-token".to_string()
        } else if let Some(var) = &entry.password_env {
            // The env-var NAME is not a secret; the value it points to is.
            format!("password_env:{var}")
        } else if entry.password.is_some() {
            "password".to_string()
        } else {
            "none".to_string()
        };
        out.push(RegistryRow {
            host: host.clone(),
            scope,
            auth,
        });
    }
}

// ---------------------------------------------------------------------------
// registry catalog — repositories in a registry
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct CatalogArgs {
    /// Registry host (default: registry.smolmachines.com)
    #[arg(value_name = "HOST")]
    pub host: Option<String>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl CatalogArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let host = self
            .host
            .unwrap_or_else(|| SMOLMACHINES_REGISTRY.to_string());
        let settings = SmolSettings::load()?;
        let config = registry_config_for(&settings, &host);
        let client = super::common::build_registry_client(&host, config, &settings.cloud)?;

        let rt = tokio::runtime::Runtime::new()?;
        let repos = rt.block_on(client.list_repositories()).map_err(|e| {
            anyhow::anyhow!(
                "could not list repositories on '{host}': {e}. \
                 Note: not every registry exposes the catalog endpoint."
            )
        })?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&repos)?);
        } else if repos.is_empty() {
            println!("(no repositories)");
        } else {
            for r in repos {
                println!("{r}");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// registry tags — tags of a repository
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct TagsArgs {
    /// Repository reference, e.g. `library/alpine` or
    /// `registry.smolmachines.com/library/alpine`.
    #[arg(value_name = "REFERENCE")]
    pub reference: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl TagsArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let parsed = smolvm::registry::Reference::parse(&self.reference)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let settings = SmolSettings::load()?;
        let config = registry_config_for(&settings, &parsed.registry);
        let client =
            super::common::build_registry_client(&parsed.registry, config, &settings.cloud)?;

        let repo = parsed.repository();
        let rt = tokio::runtime::Runtime::new()?;
        let tags = rt.block_on(client.list_tags(&repo))?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&tags)?);
        } else if tags.is_empty() {
            println!("(no tags)");
        } else {
            for t in tags {
                println!("{t}");
            }
        }
        Ok(())
    }
}

/// Pick the credential section that knows about `host`.
///
/// Artifact registries live in `[machines]` and base-image registries in
/// `[images]`; a browse command doesn't know which the user means, so prefer
/// `[images]` only when it's the sole section holding the host, and otherwise
/// fall back to `[machines]` (which also covers the smolmachines default).
fn registry_config_for<'a>(settings: &'a SmolSettings, host: &str) -> &'a RegistryConfig {
    if !settings.machines.registries.contains_key(host)
        && settings.images.registries.contains_key(host)
    {
        &settings.images
    } else {
        &settings.machines
    }
}
