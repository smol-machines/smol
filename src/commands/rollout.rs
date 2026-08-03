//! Framework-aware fused rollout administration.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use reqwest::{Client, StatusCode, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_API_URL: &str = "http://127.0.0.1:8080/api/v1";
const MAX_ADAPTER_FILES: usize = 4096;
const MAX_ADAPTER_BYTES: u64 = 32 * 1024 * 1024 * 1024;

#[derive(Args, Debug)]
pub struct RolloutCmd {
    /// Local smolvm API root.
    #[arg(long, global = true, default_value = DEFAULT_API_URL)]
    api_url: String,

    #[command(subcommand)]
    command: RolloutSubcommand,
}

#[derive(Subcommand, Debug)]
enum RolloutSubcommand {
    /// Manage fused generation backends.
    Executor(ExecutorCmd),
    /// Publish and retire immutable LoRA policy versions.
    Policy(PolicyCmd),
}

#[derive(Args, Debug)]
struct ExecutorCmd {
    #[command(subcommand)]
    command: ExecutorSubcommand,
}

#[derive(Subcommand, Debug)]
enum ExecutorSubcommand {
    /// Create a vLLM executor or verify its existing configuration.
    Ensure(EnsureExecutorCmd),
    /// List registered executors.
    Ls,
    /// Show one executor.
    Status { name: String },
    /// Drain, unload, and remove an executor.
    #[command(visible_alias = "delete")]
    Rm { name: String },
}

#[derive(Args, Debug)]
struct EnsureExecutorCmd {
    /// Stable executor name.
    name: String,
    /// Loopback vLLM endpoint with runtime LoRA enabled.
    #[arg(long)]
    endpoint: String,
    /// Host directory containing publishable adapter directories.
    #[arg(long)]
    adapter_root: PathBuf,
    /// Private Unix socket for device-resident LoRA handoff.
    #[arg(long)]
    device_adapter_socket: Option<PathBuf>,
    /// Ordinary isolated fork pool used for unsupported requests.
    #[arg(long)]
    fallback_pool: Option<String>,
    /// Maximum backend requests in flight.
    #[arg(long, default_value_t = 32)]
    max_concurrent_requests: u32,
    /// Maximum requests waiting for backend capacity.
    #[arg(long, default_value_t = 256)]
    max_queue_depth: u32,
    /// Default whole-request deadline.
    #[arg(long, default_value_t = 300)]
    request_timeout_secs: u64,
}

#[derive(Args, Debug)]
struct PolicyCmd {
    #[command(subcommand)]
    command: PolicySubcommand,
}

#[derive(Subcommand, Debug)]
enum PolicySubcommand {
    /// Verify and atomically publish an immutable adapter version.
    Publish(PublishPolicyCmd),
    /// Stop routing a policy version and unload it after requests drain.
    Retire {
        #[arg(long)]
        executor: String,
        #[arg(long)]
        policy: String,
        #[arg(long)]
        version: String,
    },
}

#[derive(Args, Debug)]
struct PublishPolicyCmd {
    #[arg(long)]
    executor: String,
    #[arg(long)]
    policy: String,
    #[arg(long)]
    version: String,
    /// Adapter directory beneath the executor's configured adapter root.
    #[arg(long)]
    adapter: PathBuf,
    /// Keep the previous current version routable.
    #[arg(long)]
    retain_previous: bool,
}

impl RolloutCmd {
    pub fn run(self) -> Result<()> {
        validate_api_url(&self.api_url)?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60 * 60))
            .build()?;
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(self.run_async(client))
    }

    async fn run_async(self, client: Client) -> Result<()> {
        let root = self.api_url.trim_end_matches('/');
        match self.command {
            RolloutSubcommand::Executor(command) => match command.command {
                ExecutorSubcommand::Ensure(command) => {
                    validate_name("executor", &command.name)?;
                    let adapter_root = command
                        .adapter_root
                        .canonicalize()
                        .with_context(|| format!("resolve {}", command.adapter_root.display()))?;
                    if !adapter_root.is_dir() {
                        anyhow::bail!("adapter root must be a directory");
                    }
                    let device_adapter_socket = command
                        .device_adapter_socket
                        .as_ref()
                        .map(|path| {
                            path.canonicalize()
                                .with_context(|| format!("resolve {}", path.display()))
                        })
                        .transpose()?;
                    let desired = json!({
                        "name": command.name,
                        "backend": "vllm",
                        "endpoint": command.endpoint,
                        "adapterRoot": adapter_root,
                        "deviceAdapterSocket": device_adapter_socket,
                        "fallbackPool": command.fallback_pool,
                        "maxConcurrentRequests": command.max_concurrent_requests,
                        "maxQueueDepth": command.max_queue_depth,
                        "requestTimeoutSecs": command.request_timeout_secs,
                    });
                    let response = client
                        .post(format!("{root}/rollout-executors"))
                        .json(&desired)
                        .send()
                        .await?;
                    if response.status() == StatusCode::CONFLICT {
                        let current = request_json(
                            client.get(format!("{root}/rollout-executors/{}", command.name)),
                        )
                        .await?;
                        ensure_same_config(&current, &desired, &command.name)?;
                        print_json(&current)?;
                    } else {
                        print_json(&decode_response(response).await?)?;
                    }
                }
                ExecutorSubcommand::Ls => {
                    print_json(
                        &request_json(client.get(format!("{root}/rollout-executors"))).await?,
                    )?;
                }
                ExecutorSubcommand::Status { name } => {
                    validate_name("executor", &name)?;
                    print_json(
                        &request_json(client.get(format!("{root}/rollout-executors/{name}")))
                            .await?,
                    )?;
                }
                ExecutorSubcommand::Rm { name } => {
                    validate_name("executor", &name)?;
                    request_empty(client.delete(format!("{root}/rollout-executors/{name}")))
                        .await?;
                    println!("Removed rollout executor '{name}'");
                }
            },
            RolloutSubcommand::Policy(command) => match command.command {
                PolicySubcommand::Publish(command) => {
                    validate_name("executor", &command.executor)?;
                    validate_name("policy", &command.policy)?;
                    validate_name("version", &command.version)?;
                    let info = request_json(
                        client.get(format!("{root}/rollout-executors/{}", command.executor)),
                    )
                    .await?;
                    let adapter_root = PathBuf::from(required_string(&info, "adapterRoot")?)
                        .canonicalize()
                        .context("resolve executor adapter root")?;
                    let adapter = command
                        .adapter
                        .canonicalize()
                        .with_context(|| format!("resolve {}", command.adapter.display()))?;
                    let relative = adapter.strip_prefix(&adapter_root).map_err(|_| {
                        anyhow::anyhow!(
                            "adapter {} must be beneath executor root {}",
                            adapter.display(),
                            adapter_root.display()
                        )
                    })?;
                    if relative.as_os_str().is_empty() || !adapter.is_dir() {
                        anyhow::bail!("adapter must be a child directory beneath the adapter root");
                    }
                    let digest = adapter_sha256(&adapter)?;
                    let body = json!({
                        "policy": command.policy,
                        "version": command.version,
                        "adapterPath": wire_path(relative),
                        "adapterSha256": digest,
                        "retainPrevious": command.retain_previous,
                    });
                    print_json(
                        &request_json(
                            client
                                .post(format!(
                                    "{root}/rollout-executors/{}/policies",
                                    command.executor
                                ))
                                .json(&body),
                        )
                        .await?,
                    )?;
                }
                PolicySubcommand::Retire {
                    executor,
                    policy,
                    version,
                } => {
                    validate_name("executor", &executor)?;
                    validate_name("policy", &policy)?;
                    validate_name("version", &version)?;
                    request_empty(client.delete(format!(
                        "{root}/rollout-executors/{executor}/policies/{policy}/{version}"
                    )))
                    .await?;
                    println!("Retired rollout policy '{policy}:{version}'");
                }
            },
        }
        Ok(())
    }
}

fn validate_api_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("parse --api-url")?;
    let loopback = match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false),
        None => false,
    };
    if url.scheme() != "http" || !loopback || url.port_or_known_default().is_none() {
        anyhow::bail!("--api-url must be an http:// loopback URL");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("--api-url cannot contain a query or fragment");
    }
    Ok(())
}

fn validate_name(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("{kind} must be 1-128 ASCII letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

async fn request_json(request: reqwest::RequestBuilder) -> Result<Value> {
    decode_response(request.send().await?).await
}

async fn request_empty(request: reqwest::RequestBuilder) -> Result<()> {
    decode_response(request.send().await?).await.map(|_| ())
}

async fn decode_response(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let bytes = response.bytes().await?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    if !status.is_success() {
        let code = value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("HTTP_ERROR");
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        anyhow::bail!("{code}: {message} (HTTP {status})");
    }
    Ok(value)
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("rollout response is missing '{key}'"))
}

fn ensure_same_config(current: &Value, desired: &Value, name: &str) -> Result<()> {
    let missing = Value::Null;
    for key in [
        "backend",
        "endpoint",
        "adapterRoot",
        "deviceAdapterSocket",
        "fallbackPool",
        "maxConcurrentRequests",
        "maxQueueDepth",
        "requestTimeoutSecs",
    ] {
        if current.get(key).unwrap_or(&missing) != desired.get(key).unwrap_or(&missing) {
            anyhow::bail!("rollout executor '{name}' exists with different configuration ({key})");
        }
    }
    Ok(())
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn wire_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn adapter_sha256(root: &Path) -> Result<String> {
    fn walk(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(current).context("read adapter directory")? {
            let entry = entry.context("read adapter directory entry")?;
            let kind = entry.file_type().context("read adapter file type")?;
            if kind.is_symlink() {
                anyhow::bail!(
                    "adapter directory contains symlink '{}'",
                    entry.path().display()
                );
            }
            if kind.is_dir() {
                walk(root, &entry.path(), files)?;
            } else if kind.is_file() {
                entry
                    .path()
                    .strip_prefix(root)
                    .context("adapter file escaped root")?;
                files.push(entry.path());
                if files.len() > MAX_ADAPTER_FILES {
                    anyhow::bail!("adapter contains more than {MAX_ADAPTER_FILES} files");
                }
            } else {
                anyhow::bail!("adapter directory contains a non-regular file");
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    if files.is_empty() {
        anyhow::bail!("adapter directory contains no files");
    }
    files.sort_by_key(|path| wire_path(path.strip_prefix(root).unwrap_or(path)));
    let mut total = 0u64;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    for file in files {
        let relative = wire_path(
            file.strip_prefix(root)
                .context("adapter file escaped root")?,
        );
        let metadata = std::fs::metadata(&file).context("stat adapter file")?;
        total = total
            .checked_add(metadata.len())
            .context("adapter byte count overflowed")?;
        if total > MAX_ADAPTER_BYTES {
            anyhow::bail!("adapter exceeds {MAX_ADAPTER_BYTES} bytes");
        }
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update(metadata.len().to_le_bytes());
        let mut input = std::fs::File::open(&file).context("open adapter file")?;
        loop {
            let count = input.read(&mut buffer).context("hash adapter file")?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_digest_matches_engine_and_sdk_contract() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("adapter_config.json"), b"{}").unwrap();
        std::fs::write(root.path().join("adapter_model.safetensors"), b"weights").unwrap();
        assert_eq!(
            adapter_sha256(root.path()).unwrap(),
            "26d1c7593b9650cb489a9a1fe2fad9def32c75ec2685cf8261c3c0fa3b73e315"
        );
    }

    #[test]
    fn api_url_is_loopback_only() {
        assert!(validate_api_url(DEFAULT_API_URL).is_ok());
        assert!(validate_api_url("http://localhost:9000/api/v1").is_ok());
        assert!(validate_api_url("http://10.0.0.1:9000/api/v1").is_err());
        assert!(validate_api_url("https://127.0.0.1:9000/api/v1").is_err());
    }

    #[test]
    fn executor_config_compares_device_handoff_socket() {
        let desired = json!({
            "backend": "vllm",
            "endpoint": "http://127.0.0.1:8000",
            "adapterRoot": "/adapters",
            "deviceAdapterSocket": "/run/smolvm/device-lora.sock",
            "fallbackPool": null,
            "maxConcurrentRequests": 32,
            "maxQueueDepth": 256,
            "requestTimeoutSecs": 300,
        });
        assert!(ensure_same_config(&desired, &desired, "qwen").is_ok());
        let mut different = desired.clone();
        different["deviceAdapterSocket"] = json!("/run/smolvm/other.sock");
        assert!(ensure_same_config(&different, &desired, "qwen").is_err());
    }
}
