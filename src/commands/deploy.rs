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

    /// Let ANY signed-in smolmachines user reach the app's URL. Without
    /// `--public` the URL works only for you, the owner. Either way the app sits
    /// behind a smolmachines login — it is never reachable anonymously.
    #[arg(long)]
    pub public: bool,

    /// Push a local .smolmachine file before deploying
    #[arg(short = 'f', long, value_name = "PATH")]
    pub file: Option<PathBuf>,
}

/// Sync-resolved inputs for the push step. Resolved outside the runtime so
/// the silent-refresh path inside `build_registry_client` is free to run its
/// own `block_on` without nesting under our outer runtime.
struct PushInputs {
    client: smolvm_registry::RegistryClient,
    reference: smolvm::registry::Reference,
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
            Some(PushInputs {
                client,
                reference: parsed,
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
            eprintln!("Pushing {} to registry...", file.display());
            let repo = inputs.reference.repository();
            let tag = inputs.reference.tag.as_deref().unwrap_or("latest");
            smolvm_registry::push(&inputs.client, &repo, tag, file).await?;
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
        let name = repo.rsplit('/').next().unwrap_or(repo.as_str());
        if name.is_empty() {
            anyhow::bail!("invalid reference: name cannot be empty");
        }
        let network = if self.network {
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

        let body = serde_json::json!({
            "name": name,
            "source": source,
            "resources": {
                "cpus": self.cpus,
                "memoryMb": self.memory,
            },
            "network": network,
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

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("deploy failed ({}): {}", status, text);
        }

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
            let text = start_resp.text().await.unwrap_or_default();
            anyhow::bail!("machine created but start failed: {}", text);
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
