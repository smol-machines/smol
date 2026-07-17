//! smol deploy — deploy a .smolmachine artifact to smolfleet.

use super::cloud;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct DeployCmd {
    /// Artifact reference, e.g. `myapp:v1` or
    /// `registry.smolmachines.com/library/alpine:latest`.
    #[arg(value_name = "REFERENCE")]
    pub reference: Option<String>,

    /// Deprecated alias — pass the reference positionally instead.
    #[arg(
        short = 'r',
        long = "ref",
        value_name = "REFERENCE",
        conflicts_with = "reference",
        hide = true
    )]
    pub ref_flag: Option<String>,

    /// Machine name (default: derived from the artifact, e.g. `alpine`). Set a
    /// distinct name to run several machines from the same image.
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Service port inside the VM
    #[arg(long, default_value = "8080")]
    pub port: u16,

    /// Number of vCPUs
    #[arg(long, default_value = "1")]
    pub cpus: u8,

    /// Memory in MiB
    #[arg(long, default_value = "512")]
    pub memory: u32,

    /// Enable outbound networking
    #[arg(long)]
    pub network: bool,

    /// Scope egress to these CIDR ranges (repeatable). Implies `--network`;
    /// the machine can reach only the listed CIDRs (plus any `--allow-host`).
    #[arg(long = "allow-cidr", value_name = "CIDR")]
    pub allow_cidr: Vec<String>,

    /// Scope egress to these hostnames and their subdomains (repeatable).
    /// Implies `--network`; the machine can reach only the listed hosts
    /// (plus any `--allow-cidr`). Example: `--allow-host api.anthropic.com`.
    #[arg(long = "allow-host", value_name = "HOSTNAME")]
    pub allow_host: Vec<String>,

    /// Let ANY signed-in smolmachines user reach the app's URL. Without
    /// `--public` the URL works only for you, the owner. Either way the app sits
    /// behind a smolmachines login — it is never reachable anonymously.
    #[arg(long)]
    pub public: bool,

    /// Push a local .smolmachine file before deploying
    #[arg(short = 'f', long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Set an environment variable in the deployed machine (KEY=VALUE, repeatable)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Inject a secret from a host env var (GUEST_VAR=HOST_VAR), resolved on the
    /// host at deploy time; the value is set in the machine, never persisted to
    /// disk on the host (repeatable)
    #[arg(long = "secret-env", value_name = "GUEST_VAR=HOST_VAR")]
    pub secret_env: Vec<String>,

    /// Inject a secret from a host file (GUEST_VAR=/abs/path), resolved on the
    /// host at deploy time (repeatable)
    #[arg(long = "secret-file", value_name = "GUEST_VAR=/abs/path")]
    pub secret_file: Vec<String>,
}

/// Sync-resolved inputs for the push step. Resolved outside the runtime so
/// the silent-refresh path inside `build_registry_client` is free to run its
/// own `block_on` without nesting under our outer runtime.
struct PushInputs {
    client: smolvm_registry::RegistryClient,
    reference: smolvm::registry::Reference,
    /// Tenant-namespaced repo for the push (e.g. `tenants/<id>/doom`). The
    /// registry token only grants the caller's own namespace, so a bare repo
    /// must be scoped exactly as `smol pack push` does — otherwise the push
    /// targets a path the token doesn't grant and 401s.
    repo: String,
}

impl DeployCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let reference =
            super::common::require_ref(self.reference.as_deref(), self.ref_flag.as_deref())?
                .to_string();
        // Phase 1 (sync): resolve everything that might trigger a token refresh
        // BEFORE entering the tokio runtime. `build_registry_client` and
        // `cloud_client` may spawn their own runtime+block_on to refresh an
        // expired Auth0 token; doing that inside an active runtime panics with
        // "Cannot start a runtime from within a runtime".
        let push_inputs = if self.file.is_some() {
            let parsed = smolvm::registry::Reference::parse(&reference)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            let settings = smolvm::SmolSettings::load()?;
            let client = super::common::build_registry_client(
                &parsed.registry,
                &settings.machines,
                &settings.cloud,
            )?;
            let repo = super::common::namespaced_repo(
                &parsed.registry,
                &parsed.repository(),
                &settings.machines,
            );
            Some(PushInputs {
                client,
                reference: parsed,
                repo,
            })
        } else {
            None
        };
        let (http, cloud_config) = cloud::cloud_client()?;

        // Phase 2 (async): all I/O against the registry and smolfleet.
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.run_async(push_inputs, http, cloud_config, reference))
    }

    async fn run_async(
        self,
        push_inputs: Option<PushInputs>,
        http: reqwest::Client,
        cloud_config: smolvm::settings::CloudSection,
        reference: String,
    ) -> anyhow::Result<()> {
        // If -f provided, push first then deploy
        if let (Some(ref file), Some(inputs)) = (&self.file, push_inputs) {
            let tag = inputs.reference.tag.as_deref().unwrap_or("latest");
            eprintln!("Pushing {} to {}:{}...", file.display(), inputs.repo, tag);
            smolvm_registry::push(&inputs.client, &inputs.repo, tag, file).await?;
            eprintln!("Pushed.");
        }

        let endpoint = cloud_config.endpoint()?;

        // Derive a valid machine name from the reference's repository (its last
        // path segment). The naive `split(':')` breaks on a `host:port/repo`
        // registry reference — it would yield the host (e.g. "127.0.0.1"),
        // which the server rejects for containing '.'.
        let parsed_ref = smolvm::registry::Reference::parse(&reference)
            .map_err(|e| anyhow::anyhow!("invalid reference: {}", e))?;
        let repo = parsed_ref.repository();
        // Name: explicit --name wins; otherwise derive from the artifact repo.
        let derived = repo.rsplit('/').next().unwrap_or(repo.as_str());
        let name = self.name.as_deref().map(str::trim).unwrap_or(derived);
        if name.is_empty() {
            anyhow::bail!("machine name cannot be empty");
        }
        super::common::validate_machine_name(name)?;
        let network = if !self.allow_cidr.is_empty() || !self.allow_host.is_empty() {
            serde_json::json!({"mode": "allowCidrs", "cidrs": self.allow_cidr, "hosts": self.allow_host})
        } else if self.network {
            serde_json::json!({"mode": "open"})
        } else {
            serde_json::json!({"mode": "blocked"})
        };

        // Decide the source kind. A `.smolmachine` artifact is either pushed from
        // a local file (`-f`) or referenced from the smolmachines artifact
        // registry; anything else is a plain OCI container image. This matters:
        // labelling an OCI image as a `.smolmachine` makes the control plane mint
        // a smolmachines pull token that the node then sends as a Bearer
        // credential to the image registry's token service (e.g. docker.io),
        // which rejects it ("unsupported authentication scheme: Bearer"). An
        // `image` source is pulled by the node with its image-registry
        // credentials instead.
        let is_smolmachine =
            self.file.is_some() || parsed_ref.registry == smolvm::registry::SMOLMACHINES_REGISTRY;
        let source = if is_smolmachine {
            // A `.smolmachine` is single-arch; declare its arch so the control
            // plane never schedules it onto a mismatched node. Read it from the
            // local artifact's manifest (`platform` is e.g. "linux/arm64") when
            // `-f` was given; otherwise leave it unset (no constraint).
            let mut source = serde_json::json!({
                "type": "smolmachine",
                "reference": reference,
            });
            if let Some(file) = &self.file {
                let sidecar = smolvm_pack::sidecar_path_for(file);
                let path = if sidecar.exists() {
                    sidecar
                } else {
                    file.clone()
                };
                if let Ok(manifest) = smolvm_pack::read_manifest_from_sidecar(&path) {
                    let arch = manifest
                        .platform
                        .rsplit('/')
                        .next()
                        .unwrap_or(&manifest.platform);
                    source["arch"] = serde_json::json!(arch);
                }
            }
            source
        } else {
            serde_json::json!({
                "type": "image",
                "reference": reference,
            })
        };

        // Machine env: plain --env plus host-resolved --secret-env/--secret-file.
        // Resolved on the host at deploy time; secret values are set in the
        // machine but never written to the host disk.
        let mut env_pairs = smolvm::util::parse_env_list(&self.env);
        env_pairs.extend(crate::commands::common::resolve_cli_secrets(
            &self.secret_env,
            &self.secret_file,
        )?);
        let env: std::collections::BTreeMap<String, String> = env_pairs.into_iter().collect();

        let body = serde_json::json!({
            "name": name,
            "source": source,
            "resources": {
                "cpus": self.cpus,
                "memoryMb": self.memory,
            },
            "network": network,
            "env": env,
            // Publish the service port. Send only the guest port; the control
            // plane allocates the node host port. The app's URL always requires a
            // smolmachines login: owner-only by default, any signed-in user when
            // `--public` is set (never anonymous).
            "ports": [{ "port": self.port }],
            "public": self.public,
        });

        eprintln!("Deploying {} to {}...", reference, endpoint);

        let resp = http
            .post(format!("{}/v1/machines", endpoint))
            .json(&body)
            .send()
            .await?;

        let resp = cloud::check_response(resp, "deploy").await?;

        let machine: cloud::CloudMachine = resp.json().await?;

        eprintln!(
            "Created: {} (id: {})",
            machine.name.as_deref().unwrap_or("-"),
            machine.id
        );
        eprintln!("Starting {}...", machine.id);

        let start_resp = http
            .post(format!("{}/v1/machines/{}/start", endpoint, machine.id))
            .send()
            .await?;

        if !start_resp.status().is_success() {
            let status = start_resp.status();
            let text = start_resp.text().await.unwrap_or_default();
            // Roll back the just-created machine so a failed deploy doesn't leave
            // a leaked `error`-state machine behind for the user to clean up.
            let _ = http
                .delete(format!("{}/v1/machines/{}", endpoint, machine.id))
                .send()
                .await;
            // A bare single-segment name resolves to the caller's own registry
            // namespace; if it wasn't found there, point at the public form.
            let not_found = status == reqwest::StatusCode::NOT_FOUND
                || text.to_lowercase().contains("not found");
            let hint = if not_found && is_smolmachine && !reference.contains('/') {
                format!(
                    "\nhint: '{reference}' was looked up in your own registry namespace. \
                     For a public image, use 'library/{reference}' (or a full reference \
                     like 'docker.io/library/{reference}')."
                )
            } else {
                String::new()
            };
            anyhow::bail!("deploy failed: {}{}", text.trim(), hint);
        }

        let started: cloud::CloudMachine = start_resp.json().await?;
        eprintln!(
            "Deployed: {} (id: {}, state: {})",
            machine.name.as_deref().unwrap_or("-"),
            machine.id,
            started.state
        );
        match started.url.as_deref() {
            Some(url) => {
                println!("{url}");
                if self.public {
                    eprintln!(
                        "Reachable by ANY signed-in smolmachines user — send your token as \
                         `Authorization: Bearer <smolmachines-token>`. Never anonymous."
                    );
                } else {
                    eprintln!(
                        "Reachable with YOUR smolmachines login only (owner-scoped) — send your token \
                         as `Authorization: Bearer <smolmachines-token>`. Re-deploy with `--public` \
                         to allow any smolmachines user."
                    );
                }
            }
            None => {
                let who = machine.name.as_deref().unwrap_or(&machine.id);
                eprintln!(
                    "No URL yet (the port may still be starting) — check `smol machine ls`, or reach it now with \
                     `smol machine exec --cloud --name {who}` / `smol machine shell --cloud --name {who}`."
                );
            }
        }
        Ok(())
    }
}
