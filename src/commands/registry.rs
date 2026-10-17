//! smol registry — the registry noun: what you're logged into, and what's in it.
//!
//! `ls` reports the registries you hold credentials for (both the `[machines]`
//! artifact registries and the `[images]` base-image registries) with secrets
//! described, never printed. `catalog` and `tags` browse a registry's contents
//! over the standard OCI distribution endpoints. `login`/`logout` are the
//! registry-scoped aliases of `smol auth login`/`logout`.
//!
//! The rest is the public smolmachines registry: `search` and `show` read it
//! without logging in; `mine`, `handle`, `publish`, `unpublish` and `readme`
//! manage what you push. A repository is private until you publish it, and
//! only repositories under your handle (`<handle>/<name>`) can be published.

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
    /// Search the public smolmachines registry
    Search(SearchArgs),
    /// Show a public repository: description, README, tags, pulls
    Show(ShowArgs),
    /// List the repositories you have pushed
    Mine(MineArgs),
    /// Claim the handle you publish under (chosen once)
    Handle(HandleArgs),
    /// Make a repository under your handle public
    Publish(RepoArgs),
    /// Make a repository private again
    Unpublish(RepoArgs),
    /// Set a repository's README from a Markdown file
    Readme(ReadmeArgs),
}

impl RegistryCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            RegistrySubcommand::Ls(cmd) => cmd.run(),
            RegistrySubcommand::Catalog(cmd) => cmd.run(),
            RegistrySubcommand::Tags(cmd) => cmd.run(),
            RegistrySubcommand::Login(cmd) => cmd.run(),
            RegistrySubcommand::Logout(cmd) => cmd.run(),
            RegistrySubcommand::Search(cmd) => cmd.run(),
            RegistrySubcommand::Show(cmd) => cmd.run(),
            RegistrySubcommand::Mine(cmd) => cmd.run(),
            RegistrySubcommand::Handle(cmd) => cmd.run(),
            RegistrySubcommand::Publish(cmd) => cmd.run(true),
            RegistrySubcommand::Unpublish(cmd) => cmd.run(false),
            RegistrySubcommand::Readme(cmd) => cmd.run(),
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

// ---------------------------------------------------------------------------
// The public smolmachines registry: search, show, mine, handle, publish, readme
// ---------------------------------------------------------------------------

/// Base URL of the control plane that indexes the smolmachines registry.
fn cloud_api_base() -> String {
    match std::env::var("SMOL_CLOUD_API") {
        Ok(v) if !v.is_empty() => v.trim_end_matches('/').to_string(),
        _ => smolvm::registry::SMOLMACHINES_API.to_string(),
    }
}

/// Strip an explicit `registry.smolmachines.com/` host and any `:tag` so the
/// path matches what the index stores.
fn repo_path(reference: &str) -> anyhow::Result<String> {
    let parsed =
        smolvm::registry::Reference::parse(reference).map_err(|e| anyhow::anyhow!("{}", e))?;
    if parsed.registry != SMOLMACHINES_REGISTRY {
        anyhow::bail!(
            "'{}' is on {}; publishing applies to the smolmachines registry only",
            reference,
            parsed.registry
        );
    }
    Ok(parsed.repository())
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepoInfo {
    repo: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    pulls: u64,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    updated_at: String,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepoDetail {
    #[serde(flatten)]
    info: RepoInfo,
    #[serde(default)]
    readme: String,
}

/// One line per repository: path, kind, pulls, description.
fn print_repos(repos: &[RepoInfo]) {
    if repos.is_empty() {
        println!("(nothing found)");
        return;
    }
    let w = repos.iter().map(|r| r.repo.len()).max().unwrap_or(4).max(4);
    println!(
        "{:<w$}  {:<10}  {:>6}  DESCRIPTION",
        "REPOSITORY", "KIND", "PULLS"
    );
    for r in repos {
        let desc: String = r.description.chars().take(70).collect();
        println!("{:<w$}  {:<10}  {:>6}  {}", r.repo, r.kind, r.pulls, desc);
    }
}

async fn read_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .or(v.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or(body);
    anyhow::anyhow!("{status}: {message}")
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Substring of the repository name or description
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,
    /// Only this kind: workspace, checkpoint or pack
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
    /// At most this many results
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl SearchArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        let repos: Vec<RepoInfo> = rt.block_on(async {
            let mut req = super::common::http_client()?
                .get(format!("{}/v1/registry/search", cloud_api_base()))
                .query(&[("limit", self.limit.to_string())]);
            if let Some(q) = &self.query {
                req = req.query(&[("q", q)]);
            }
            if let Some(k) = &self.kind {
                req = req.query(&[("kind", k)]);
            }
            let resp = req.send().await?;
            if !resp.status().is_success() {
                return Err(read_error(resp).await);
            }
            Ok(resp.json().await?)
        })?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&repos)?);
        } else {
            print_repos(&repos);
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Repository, e.g. `acme/kicad` or `library/alpine`
    #[arg(value_name = "REPOSITORY")]
    pub repository: String,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl ShowArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let repo = repo_path(&self.repository)?;
        let rt = tokio::runtime::Runtime::new()?;
        let detail: RepoDetail = rt.block_on(async {
            let resp = super::common::http_client()?
                .get(format!("{}/v1/registry/repos/{}", cloud_api_base(), repo))
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("no public repository '{repo}'");
            }
            if !resp.status().is_success() {
                return Err(read_error(resp).await);
            }
            Ok(resp.json().await?)
        })?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&detail)?);
            return Ok(());
        }
        let i = &detail.info;
        println!("{}", i.repo);
        if !i.description.is_empty() {
            println!("{}", i.description);
        }
        println!();
        println!("kind      {}", i.kind);
        if let Some(owner) = &i.owner {
            println!("owner     {owner}");
        }
        println!("pulls     {}", i.pulls);
        println!(
            "tags      {}",
            if i.tags.is_empty() {
                "(none)".to_string()
            } else {
                i.tags.join(", ")
            }
        );
        if !i.updated_at.is_empty() {
            println!("updated   {}", i.updated_at);
        }
        let tag = i.tags.first().map(String::as_str).unwrap_or("latest");
        println!();
        println!(
            "start     smol create --image {SMOLMACHINES_REGISTRY}/{}:{tag}",
            i.repo
        );
        if !detail.readme.trim().is_empty() {
            println!();
            println!("{}", detail.readme.trim_end());
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct MineArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MineRow {
    repo: String,
    tag: String,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    pulls: u64,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    updated_at: String,
}

impl MineArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let (http, cloud) = super::cloud::cloud_client()?;
        let endpoint = cloud.endpoint()?.to_string();
        let rt = tokio::runtime::Runtime::new()?;
        let rows: Vec<MineRow> = rt.block_on(async {
            let resp = http
                .get(format!(
                    "{}/v1/registry/mine",
                    endpoint.trim_end_matches('/')
                ))
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(read_error(resp).await);
            }
            Ok(resp.json().await?)
        })?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        if rows.is_empty() {
            println!("You have not pushed anything yet. Push with: smol pack push <name>:<tag> -f <file>");
            return Ok(());
        }
        let w = rows.iter().map(|r| r.repo.len()).max().unwrap_or(4).max(4);
        println!(
            "{:<w$}  {:<12}  {:<10}  {:<8}  {:>6}  {:>8}",
            "REPOSITORY", "TAG", "KIND", "ACCESS", "PULLS", "SIZE"
        );
        for r in &rows {
            println!(
                "{:<w$}  {:<12}  {:<10}  {:<8}  {:>6}  {:>8}",
                r.repo,
                r.tag,
                r.kind,
                if r.public { "public" } else { "private" },
                r.pulls,
                human_bytes(r.size_bytes)
            );
        }
        Ok(())
    }
}

fn human_bytes(b: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else {
        format!("{:.0} KiB", b / 1024.0)
    }
}

#[derive(Args, Debug)]
pub struct HandleArgs {
    /// The handle to claim: 3 to 39 lowercase letters, digits and hyphens.
    /// Omit it to print the current one.
    #[arg(value_name = "HANDLE")]
    pub handle: Option<String>,
}

impl HandleArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let (http, cloud) = super::cloud::cloud_client()?;
        let endpoint = cloud.endpoint()?.trim_end_matches('/').to_string();
        let rt = tokio::runtime::Runtime::new()?;
        let me: serde_json::Value = rt.block_on(async {
            let resp = match &self.handle {
                Some(h) => {
                    http.put(format!("{endpoint}/v1/me/handle"))
                        .json(&serde_json::json!({ "handle": h }))
                        .send()
                        .await?
                }
                None => http.get(format!("{endpoint}/v1/me")).send().await?,
            };
            if !resp.status().is_success() {
                return Err(read_error(resp).await);
            }
            Ok(resp.json().await?)
        })?;
        match me.get("handle").and_then(|h| h.as_str()) {
            Some(h) => {
                println!("{h}");
                println!("Public repositories live under {h}/<name>: push as {SMOLMACHINES_REGISTRY}/{h}/<name>:<tag>, then `smol registry publish {h}/<name>`.");
            }
            None => println!("No handle yet. Claim one with: smol registry handle <name>"),
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct RepoArgs {
    /// Repository under your handle, e.g. `acme/kicad`
    #[arg(value_name = "REPOSITORY")]
    pub repository: String,
}

/// `PATCH /v1/registry/mine/<repo>` with the given body; prints the repository.
fn update_repo(repo: &str, body: serde_json::Value) -> anyhow::Result<RepoInfo> {
    let (http, cloud) = super::cloud::cloud_client()?;
    let endpoint = cloud.endpoint()?.trim_end_matches('/').to_string();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let resp = http
            .patch(format!("{endpoint}/v1/registry/mine/{repo}"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(read_error(resp).await);
        }
        Ok(resp.json().await?)
    })
}

impl RepoArgs {
    pub fn run(self, public: bool) -> anyhow::Result<()> {
        let repo = repo_path(&self.repository)?;
        let info = update_repo(&repo, serde_json::json!({ "public": public }))?;
        if info.public {
            println!(
                "{} is public: anyone can pull {SMOLMACHINES_REGISTRY}/{}; it is listed at https://smolmachines.com/machines/{}",
                info.repo, info.repo, info.repo
            );
        } else {
            println!("{} is private.", info.repo);
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct ReadmeArgs {
    /// Repository you own, e.g. `acme/kicad`
    #[arg(value_name = "REPOSITORY")]
    pub repository: String,
    /// Markdown file to set as the README (at most 64 KiB)
    #[arg(short = 'f', long, value_name = "PATH")]
    pub file: std::path::PathBuf,
}

impl ReadmeArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let repo = repo_path(&self.repository)?;
        let readme = std::fs::read_to_string(&self.file)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", self.file.display()))?;
        let info = update_repo(&repo, serde_json::json!({ "readme": readme }))?;
        println!("README set on {} ({} bytes).", info.repo, readme.len());
        Ok(())
    }
}
