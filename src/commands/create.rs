//! smol machine create — create a persistent machine.

use clap::Args;
use smolvm::network::NetworkBackend;

#[derive(Args, Debug)]
pub struct CreateCmd {
    /// Accepted so scripts can pass `--local` to every machine command; this
    /// command only manages local machines.
    #[arg(long = "local")]
    pub _local: bool,

    /// Machine name (auto-generated if omitted)
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Container image
    #[arg(short = 'I', long, value_name = "IMAGE", conflicts_with = "from")]
    pub image: Option<String>,

    /// Create from a packed .smolmachine artifact (uses its pre-extracted layers
    /// instead of pulling from a registry)
    #[arg(long, value_name = "PATH")]
    pub from: Option<std::path::PathBuf>,

    /// Number of vCPUs [default: 4, or the pack's]
    #[arg(long, value_name = "N")]
    pub cpus: Option<u8>,

    /// Memory in MiB [default: 8192, or the pack's]
    #[arg(long, value_name = "MiB")]
    pub mem: Option<u32>,

    /// Storage disk size in GiB (OCI layers and container data)
    #[arg(long, value_name = "GiB")]
    pub storage: Option<u64>,

    /// Overlay disk size in GiB (persistent rootfs changes)
    #[arg(long, value_name = "GiB")]
    pub overlay: Option<u64>,

    /// Enable GPU acceleration (Vulkan via virtio-gpu)
    #[arg(long)]
    pub gpu: bool,

    /// GPU VRAM in MiB (requires --gpu)
    #[arg(long, value_name = "MiB")]
    pub gpu_vram: Option<u32>,

    /// Remote guest CUDA Driver-API calls to the host NVIDIA GPU over vsock
    #[arg(long, help_heading = "Hardware")]
    pub cuda: bool,

    /// Workload command to run when the machine starts (overrides the image's
    /// entrypoint/cmd, matching the engine's `machine create -- <command>`).
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,

    /// Mount a host directory. `:staged` uses a guest-local working copy until sync or stop.
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro|rw|staged]")]
    pub volume: Vec<String>,

    /// Expose port (HOST:GUEST, append /udp for UDP)
    #[arg(short = 'p', long = "port", value_parser = parse_port, value_name = "HOST:GUEST[/udp]")]
    pub port: Vec<String>,

    /// Attach metadata to the machine (repeatable), e.g.
    /// `--label owner=ci --label sandbox=agent-7`.
    ///
    /// Never interpreted: labels exist so a process managing many machines can
    /// recognise its own later, instead of encoding that into the name. Read
    /// them back with `smol machine ls --json`.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub label: Vec<String>,

    /// Enable outbound network access
    #[arg(long)]
    pub net: bool,

    /// Allow egress only to this CIDR (repeatable; implies --net). e.g. 10.0.0.0/8
    #[arg(long = "allow-cidr", value_name = "CIDR")]
    pub allow_cidr: Vec<String>,

    /// Allow egress only to this hostname's IPs, resolved at start (repeatable;
    /// implies --net)
    #[arg(long = "allow-host", value_name = "HOSTNAME")]
    pub allow_host: Vec<String>,

    /// Restrict egress to localhost only (implies --net)
    #[arg(long)]
    pub outbound_localhost_only: bool,

    /// Network backend (tsi or virtio-net)
    #[arg(long = "net-backend", value_enum, value_name = "BACKEND")]
    pub net_backend: Option<NetworkBackend>,

    /// Disk durability: `full` (default), a guest fsync waits for the host's
    /// disk; `deferred`, guest writes reach the host's disk on the host's
    /// schedule, so database commits and other fsync-heavy work run far faster
    /// and a host crash can lose the most recent writes.
    #[arg(long = "disk-durability", value_enum, value_name = "MODE")]
    pub disk_durability: Option<smolvm::data::resources::DiskDurability>,

    /// Forward the host SSH agent into the machine
    #[arg(long)]
    pub ssh_agent: bool,

    /// Command to run once, on the machine's first start (repeatable)
    #[arg(long = "init", value_name = "COMMAND")]
    pub init: Vec<String>,

    /// Set environment variable (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Set working directory
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Run the workload as this user, like `docker run --user`: a name from the
    /// image or a numeric `uid[:gid]`. Overrides the image's USER, so a workload
    /// can match the owner of a mounted host directory.
    #[arg(short = 'u', long, value_name = "USER")]
    pub user: Option<String>,

    /// Inject a secret from a host env var (GUEST_VAR=HOST_VAR), resolved at
    /// each start/exec; only the reference is stored, never the plaintext
    #[arg(long = "secret-env", value_name = "GUEST_VAR=HOST_VAR")]
    pub secret_env: Vec<String>,

    /// Inject a secret from a host file (GUEST_VAR=/abs/path), resolved at each
    /// start/exec; only the reference is stored, never the plaintext
    #[arg(long = "secret-file", value_name = "GUEST_VAR=PATH")]
    pub secret_file: Vec<String>,
}

impl CreateCmd {
    /// Create the machine with the engine's own `machine create`: the flags
    /// here are a subset of its flags, handed to it unchanged, so a pack, a
    /// checkpoint, an image and a bare machine apply every option by the one
    /// set of rules the engine uses on every creation path, and an option a
    /// source cannot take is refused by name instead of dropped.
    pub fn run(self) -> anyhow::Result<()> {
        let record = smolvm_create(self.engine_args())?;
        println!("Created machine: {}", record.name);
        println!("  CPUs: {}, Memory: {} MiB", record.cpus, record.mem);
        println!(
            "\nUse 'smol machine start --name {}' to start the machine",
            record.name
        );
        Ok(())
    }

    /// The engine's `machine create` arguments for these flags. Every field is
    /// named, so a new flag does not compile until it is passed on.
    fn engine_args(self) -> Vec<String> {
        let CreateCmd {
            _local: _,
            name,
            image,
            from,
            cpus,
            mem,
            storage,
            overlay,
            gpu,
            gpu_vram,
            cuda,
            command,
            volume,
            port,
            label,
            net,
            allow_cidr,
            allow_host,
            outbound_localhost_only,
            net_backend,
            disk_durability,
            ssh_agent,
            init,
            env,
            workdir,
            user,
            secret_env,
            secret_file,
        } = self;
        let mut args: Vec<String> = Vec::new();
        let mut value = |flag: &str, value: Option<String>| {
            if let Some(value) = value {
                args.push(flag.to_string());
                args.push(value);
            }
        };
        value("--name", name);
        value("--image", image);
        value("--from", from.map(|p| p.to_string_lossy().into_owned()));
        value("--cpus", cpus.map(|v| v.to_string()));
        value("--mem", mem.map(|v| v.to_string()));
        value("--storage", storage.map(|v| v.to_string()));
        value("--overlay", overlay.map(|v| v.to_string()));
        value("--gpu-vram", gpu_vram.map(|v| v.to_string()));
        value("--net-backend", net_backend.map(net_backend_name));
        value(
            "--disk-durability",
            disk_durability.map(|d| {
                match d {
                    smolvm::data::resources::DiskDurability::Full => "full",
                    smolvm::data::resources::DiskDurability::Deferred => "deferred",
                }
                .to_string()
            }),
        );
        value("--workdir", workdir);
        value("--user", user);
        for (flag, values) in [
            ("--volume", volume),
            ("--port", port),
            ("--label", label),
            ("--allow-cidr", allow_cidr),
            ("--allow-host", allow_host),
            ("--init", init),
            ("--env", env),
            ("--secret-env", secret_env),
            ("--secret-file", secret_file),
        ] {
            for v in values {
                value(flag, Some(v));
            }
        }
        for (flag, on) in [
            ("--gpu", gpu),
            ("--cuda", cuda),
            ("--net", net),
            ("--outbound-localhost-only", outbound_localhost_only),
            ("--ssh-agent", ssh_agent),
        ] {
            if on {
                args.push(flag.to_string());
            }
        }
        if !command.is_empty() {
            args.push("--".to_string());
            args.extend(command);
        }
        args
    }
}

fn net_backend_name(backend: NetworkBackend) -> String {
    <NetworkBackend as clap::ValueEnum>::to_possible_value(&backend)
        .map(|v| v.get_name().to_string())
        .unwrap_or_default()
}

/// Validate a `-p` value the way the engine will parse it, keeping the text.
fn parse_port(value: &str) -> Result<String, String> {
    smolvm::data::network::PortMappingSpec::parse(value).map(|_| value.to_string())
}

/// Run the engine's `machine create` with `args` and return the record.
fn smolvm_create(args: Vec<String>) -> anyhow::Result<smolvm::VmRecord> {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "smol")]
    struct Engine {
        #[command(subcommand)]
        command: smolvm::cli::machine::MachineCmd,
    }
    let argv = ["smol", "create"].into_iter().map(String::from).chain(args);
    let smolvm::cli::machine::MachineCmd::Create(create) = Engine::try_parse_from(argv)?.command
    else {
        unreachable!("the arguments name machine create");
    };
    Ok(create.create()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        create: CreateCmd,
    }

    fn engine_args(argv: &[&str]) -> Vec<String> {
        let mut full = vec!["smol"];
        full.extend_from_slice(argv);
        Harness::try_parse_from(full).unwrap().create.engine_args()
    }

    #[test]
    fn every_flag_reaches_the_engine_create() {
        let args = engine_args(&[
            "-n",
            "m",
            "--from",
            "/a.smolmachine",
            "--cpus",
            "2",
            "--mem",
            "512",
            "--storage",
            "30",
            "--overlay",
            "5",
            "--gpu",
            "--gpu-vram",
            "1024",
            "--cuda",
            "-v",
            "/a:/b",
            "-p",
            "8080:80/udp",
            "--label",
            "k=v",
            "--net",
            "--allow-cidr",
            "10.0.0.0/8",
            "--allow-host",
            "example.com",
            "--outbound-localhost-only",
            "--net-backend",
            "virtio-net",
            "--disk-durability",
            "deferred",
            "--ssh-agent",
            "--init",
            "echo hi",
            "-e",
            "A=1",
            "-w",
            "/w",
            "-u",
            "1000",
            "--secret-env",
            "T=H",
            "--secret-file",
            "F=/f",
            "sleep",
            "1",
        ]);
        let joined = args.join(" ");
        for expected in [
            "--name m",
            "--from /a.smolmachine",
            "--cpus 2",
            "--mem 512",
            "--storage 30",
            "--overlay 5",
            "--gpu-vram 1024",
            "--net-backend virtio-net",
            "--disk-durability deferred",
            "--workdir /w",
            "--user 1000",
            "--volume /a:/b",
            "--port 8080:80/udp",
            "--label k=v",
            "--allow-cidr 10.0.0.0/8",
            "--allow-host example.com",
            "--init echo hi",
            "--env A=1",
            "--secret-env T=H",
            "--secret-file F=/f",
            "--gpu",
            "--cuda",
            "--net",
            "--outbound-localhost-only",
            "--ssh-agent",
            "-- sleep 1",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
        // And the engine accepts every one of them.
        use clap::Parser as _;
        #[derive(Parser)]
        struct Engine {
            #[command(subcommand)]
            command: smolvm::cli::machine::MachineCmd,
        }
        let argv = ["smol", "create"].into_iter().map(String::from).chain(args);
        assert!(Engine::try_parse_from(argv).is_ok());
    }

    #[test]
    fn unset_resources_stay_unset_so_a_pack_keeps_its_own() {
        let args = engine_args(&["--from", "/a.smolmachine"]);
        assert!(!args.iter().any(|a| a == "--cpus" || a == "--mem"));
    }
}
