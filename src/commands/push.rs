//! smol push — push a .smolmachine to a registry.

use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct PushCmd {
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

    /// Path to the .smolmachine file
    #[arg(short = 'f', long, value_name = "PATH")]
    pub file: PathBuf,
}

impl PushCmd {
    pub fn run(self) -> anyhow::Result<()> {
        if !self.file.exists() {
            anyhow::bail!("file not found: {}", self.file.display());
        }

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

        // For the smolmachines registry, scope a bare repo under the caller's
        // tenant (`tenants/<tenant>/<name>`) — the registry token only grants the
        // tenant's own namespace, and this is the repo the control plane will
        // reference on deploy.
        let repo = super::common::namespaced_repo(
            &parsed.registry,
            &parsed.repository(),
            &settings.machines,
        );
        let tag = parsed.tag.as_deref().unwrap_or("latest");

        tracing::info!(registry = %parsed.registry, repo = %repo, tag, file = %self.file.display(), "starting push");

        eprintln!(
            "Pushing {} to {}/{}:{}",
            self.file.display(),
            parsed.registry,
            repo,
            tag,
        );

        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(smolvm_registry::push(&client, &repo, tag, &self.file))?;

        tracing::debug!(
            layer_digest = %result.layer_digest,
            layer_size = result.layer_size,
            manifest_digest = %result.manifest_digest,
            "push completed"
        );

        eprintln!(
            "Pushed successfully\n  Layer:    {} ({} bytes)\n  Manifest: {}",
            result.layer_digest, result.layer_size, result.manifest_digest,
        );

        // For the smolmachines registry, register the artifact in the tenant's
        // catalog (`POST /v1/registry/machines`) so it shows in `smol registry`
        // and the console. The blob is already pushed; registration is
        // best-effort — warn but do not fail the push on error.
        if parsed.registry == smolvm::registry::SMOLMACHINES_REGISTRY {
            match register_in_catalog(&rt, &client, &repo, tag, &result, &self.file) {
                Ok(()) => eprintln!(
                    "Registered in your catalog — visible via `smol registry` and in the console."
                ),
                Err(e) => eprintln!(
                    "warning: pushed, but failed to register in your catalog \
                     (it may not appear in `smol registry` / the console): {e}"
                ),
            }
        }

        Ok(())
    }
}

/// Register a just-pushed smolmachines artifact in the caller's catalog
/// (`POST /v1/registry/machines`) so it appears in `smol registry` + the
/// console. Best-effort; the blob is already in the registry regardless.
fn register_in_catalog(
    rt: &tokio::runtime::Runtime,
    reg_client: &smolvm_registry::RegistryClient,
    repo: &str,
    tag: &str,
    result: &smolvm_registry::PushResult,
    file: &std::path::Path,
) -> anyhow::Result<()> {
    // `cloud_client()` may refresh the token via its own runtime — call it in
    // sync context (outside our block_on) to avoid nesting runtimes.
    let (http, cloud_config) = super::cloud::cloud_client()?;
    let endpoint = cloud_config.endpoint()?.to_string();

    // Read the pushed (possibly multi-arch) index for its digest + all
    // platforms, so a later single-arch push doesn't narrow the catalog entry.
    let (digest, platforms) = rt.block_on(async {
        let (bytes, digest) = reg_client.get_manifest_raw(repo, tag).await?;
        let platforms =
            index_platforms(&bytes).unwrap_or_else(|| vec![result.platform.clone()]);
        anyhow::Ok((digest, platforms))
    })?;

    let manifest = pack_manifest_json(file);
    let body = serde_json::json!({
        "repo": repo,
        "tag": tag,
        "digest": digest,
        "size_bytes": result.layer_size,
        "platforms": platforms,
        "manifest": manifest,
    });

    rt.block_on(async {
        let resp = http
            .post(format!("{}/v1/registry/machines", endpoint))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("control plane returned {} {}", status.as_u16(), detail);
        }
        anyhow::Ok(())
    })
}

/// Extract `os/architecture` strings from an OCI image index; `None` for a
/// single (non-index) manifest.
fn index_platforms(bytes: &[u8]) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let manifests = v.get("manifests")?.as_array()?;
    let out: Vec<String> = manifests
        .iter()
        .filter_map(|m| {
            let p = m.get("platform")?;
            let os = p.get("os")?.as_str()?;
            let arch = p.get("architecture")?.as_str()?;
            Some(format!("{os}/{arch}"))
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Read the pack manifest from the artifact's sidecar and serialize it to a JSON
/// string for the catalog (opaque to the control plane; the UI parses it).
/// Falls back to `{}` when unavailable.
fn pack_manifest_json(file: &std::path::Path) -> String {
    let sidecar = smolvm_pack::sidecar_path_for(file);
    let path = if sidecar.exists() {
        sidecar
    } else {
        file.to_path_buf()
    };
    smolvm_pack::read_manifest_from_sidecar(&path)
        .ok()
        .and_then(|m| serde_json::to_string(&m).ok())
        .unwrap_or_else(|| "{}".to_string())
}
