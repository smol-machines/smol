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
        Ok(())
    }
}
