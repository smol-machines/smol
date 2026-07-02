//! smol scale — scale a group on smolfleet.

use clap::Args;

#[derive(Args, Debug)]
pub struct ScaleCmd {
    /// Group name
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Target replica count (declarative — sets to this number)
    #[arg(long)]
    pub count: u32,
}

impl ScaleCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // `smol scale` sets a deployed group's replica count via
        // POST /v1/groups/{name}/scale (groups are scaled by name, not machine id).
        // Phase 1 (sync): resolve credentials BEFORE entering any runtime —
        // `cloud_client` may spawn its own runtime to refresh an expired token.
        let (http, cloud_config) = super::cloud::cloud_client()?;
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async move {
            let endpoint = cloud_config.endpoint()?;
            let body = serde_json::json!({ "count": self.count });

            let resp = http
                .post(format!("{}/v1/groups/{}/scale", endpoint, self.name))
                .json(&body)
                .send()
                .await?;

            match resp.status().as_u16() {
                200..=299 => eprintln!("Scaled group '{}' to {} replicas", self.name, self.count),
                404 => anyhow::bail!("group '{}' not found", self.name),
                _ => {
                    super::cloud::check_response(resp, "scale group").await?;
                }
            }

            Ok::<(), anyhow::Error>(())
        })
    }
}
