//! smol pull — pull a .smolmachine from a registry.

use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct PullCmd {
    /// Artifact reference, e.g. `alpine:latest` or
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

    /// Output path for the .smolmachine file
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

impl PullCmd {
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

        let repo = parsed.repository();
        let tag_or_digest = parsed
            .digest
            .as_deref()
            .or(parsed.tag.as_deref())
            .unwrap_or("latest");

        tracing::info!(registry = %parsed.registry, repo = %repo, reference = tag_or_digest, "starting pull");

        eprintln!("Pulling {}/{}:{}", parsed.registry, repo, tag_or_digest);

        let rt = tokio::runtime::Runtime::new()?;
        let cache = smolvm_registry::BlobCache::open_default()?;

        let result = rt.block_on(smolvm_registry::pull(
            &client,
            &repo,
            tag_or_digest,
            self.output.as_deref(),
            &cache,
            &[], // no brokered P2P peers for a CLI pull
        ))?;

        tracing::debug!(digest = %result.digest, size = result.size, cached = result.cached, "pull completed");

        if result.cached {
            eprintln!("Using cached blob ({})", result.digest);
        }

        let dest = self.output.unwrap_or(result.path);
        eprintln!(
            "Pulled successfully -> {} ({} bytes)",
            dest.display(),
            result.size,
        );
        Ok(())
    }
}
