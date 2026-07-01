//! smol file init — create a Smolfile in the current directory.

use clap::Args;
use std::path::Path;

#[derive(Args, Debug)]
pub struct InitCmd {
    /// Pre-fill image field
    #[arg(long, value_name = "IMAGE")]
    pub image: Option<String>,

    /// Use built-in template (python, node, go)
    #[arg(long, value_name = "TEMPLATE")]
    pub template: Option<String>,
}

impl InitCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let path = Path::new("Smolfile");
        if path.exists() {
            anyhow::bail!("Smolfile already exists in this directory");
        }

        let content = match self.template.as_deref() {
            Some("python") => TEMPLATE_PYTHON.to_string(),
            Some("node") => TEMPLATE_NODE.to_string(),
            Some("go") => TEMPLATE_GO.to_string(),
            Some(name) => anyhow::bail!("unknown template: '{}'. Available: python, node, go", name),
            None => {
                let image = self.image.as_deref().unwrap_or("alpine");
                format!(
                    r#"image = "{image}"
cpus = 2
memory = 1024
net = true

[dev]
volumes = [".:/workspace"]
# ports = ["8080:8080"]
# init = ["echo 'hello from init'"]
"#
                )
            }
        };

        std::fs::write(path, &content)?;
        println!("Created Smolfile");
        println!("\nUse 'smol file up' to start the machine it defines");
        Ok(())
    }
}

const TEMPLATE_PYTHON: &str = r#"image = "python:3.12-alpine"
workdir = "/app"
cpus = 2
memory = 1024
net = true

[dev]
volumes = ["./src:/app"]
ports = ["8080:8080"]
init = ["pip install -r requirements.txt"]
env = ["PYTHONDONTWRITEBYTECODE=1"]
"#;

const TEMPLATE_NODE: &str = r#"image = "node:22-alpine"
workdir = "/app"
cpus = 2
memory = 1024
net = true

[dev]
volumes = ["./src:/app"]
ports = ["3000:3000"]
init = ["npm install"]
env = ["NODE_ENV=development"]
"#;

const TEMPLATE_GO: &str = r#"image = "golang:1.23-alpine"
workdir = "/app"
cpus = 4
memory = 2048
net = true

[dev]
volumes = ["./src:/app"]
ports = ["8080:8080"]
init = ["go mod download"]
"#;
