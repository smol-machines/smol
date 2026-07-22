//! smol inspect — inspect a .smolmachine in a registry.

use super::common::format_bytes;
use clap::Args;

#[derive(Args, Debug)]
pub struct InspectCmd {
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

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl InspectCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let reference =
            super::common::require_ref(self.reference.as_deref(), self.ref_flag.as_deref())?;
        let parsed =
            smolvm::registry::Reference::parse(reference).map_err(|e| anyhow::anyhow!("{}", e))?;
        let settings = smolvm::SmolSettings::load()?;
        let client = super::common::build_registry_client(
            &parsed.registry,
            &settings.machines,
            &settings.cloud,
        )?;

        // Scope a bare repo under the caller's tenant on the smolmachines
        // registry, matching `pack push`/`pack pull` — otherwise a short ref
        // pushed as `tenants/<tenant>/<name>` resolves to the bare registry
        // root and 401s on inspect.
        let repo = super::common::namespaced_repo(
            &parsed.registry,
            &parsed.repository(),
            &settings.machines,
        );
        let tag_or_digest = parsed
            .digest
            .as_deref()
            .or(parsed.tag.as_deref())
            .unwrap_or("latest");

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_inspect(
            &client,
            &parsed,
            &repo,
            tag_or_digest,
            self.json,
        ))?;
        Ok(())
    }
}

async fn run_inspect(
    client: &smolvm_registry::RegistryClient,
    parsed: &smolvm::registry::Reference,
    repo: &str,
    tag_or_digest: &str,
    json_output: bool,
) -> anyhow::Result<()> {
    // Resolve an OCI image index to this host's platform manifest — the same
    // path `pull` uses — so an index-wrapped .smolmachine (what `cloud export`
    // and multi-arch packs produce) inspects instead of being rejected as a
    // "Docker image". A plain single manifest passes straight through.
    let manifest_bytes = client.get_manifest_resolved(repo, tag_or_digest).await?;
    let oci_manifest: smolvm_registry::OciManifest = serde_json::from_slice(&manifest_bytes)?;

    let layer_size = oci_manifest.layers.first().map(|l| l.size).unwrap_or(0);
    let layer_digest = oci_manifest
        .layers
        .first()
        .map(|l| l.digest.as_str())
        .unwrap_or("unknown");

    let config_bytes = client.pull_blob(repo, &oci_manifest.config.digest).await?;
    let pack_manifest: smolvm_pack::PackManifest = serde_json::from_slice(&config_bytes)?;

    if json_output {
        let mut json_val: serde_json::Value = serde_json::to_value(&pack_manifest)?;
        if let Some(obj) = json_val.as_object_mut() {
            obj.insert(
                "layer_size".to_string(),
                serde_json::Value::Number(layer_size.into()),
            );
            obj.insert(
                "layer_digest".to_string(),
                serde_json::Value::String(layer_digest.to_string()),
            );
        }
        println!("{}", serde_json::to_string_pretty(&json_val)?);
    } else {
        let full_ref = format!("{}/{}:{}", parsed.registry, repo, tag_or_digest);
        println!("Reference:  {}", full_ref);
        println!("Image:      {}", pack_manifest.image);
        println!("Platform:   {}", pack_manifest.platform);
        println!("Host:       {}", pack_manifest.host_platform);
        println!("CPUs:       {}", pack_manifest.cpus);
        println!("Memory:     {} MiB", pack_manifest.mem);
        if !pack_manifest.entrypoint.is_empty() {
            println!("Entrypoint: {}", pack_manifest.entrypoint.join(" "));
        }
        if !pack_manifest.cmd.is_empty() {
            println!("Cmd:        {}", pack_manifest.cmd.join(" "));
        }
        if let Some(ref wd) = pack_manifest.workdir {
            println!("Workdir:    {}", wd);
        }
        println!("Created:    {}", pack_manifest.created);
        println!("Version:    {}", pack_manifest.smolvm_version);
        println!("Size:       {}", format_bytes(layer_size));
        println!("Digest:     {}", layer_digest);
    }

    Ok(())
}
