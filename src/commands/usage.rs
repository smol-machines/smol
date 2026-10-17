//! smol usage — the local journal of what this install does with machines.
//!
//! The engine appends one line per lifecycle event (create, branch, start,
//! stop, pause, resume, resize, delete, checkpoint, restore) to a JSONL file
//! on this host. Machine names are hashed per install; no commands, paths,
//! image names, labels or output are recorded. Nothing is uploaded unless
//! `smol usage share on` has been run.

use clap::{Args, Subcommand};
use smolvm::settings::SmolSettings;
use smolvm::usage;

#[derive(Args, Debug)]
pub struct UsageCmd {
    #[command(subcommand)]
    pub command: UsageSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum UsageSubcommand {
    /// Summarize the journal: machines, branches, tree depth, lifetimes
    Summary(SummaryCmd),
    /// Print the most recent events as JSON lines
    Show(ShowCmd),
    /// Write the whole journal to a file (JSON lines)
    Export(ExportCmd),
    /// Where the journal lives, and whether recording and sharing are on
    Status,
    /// Turn local recording on
    On,
    /// Turn local recording off
    Off,
    /// Upload recorded events: on, off, now, status
    Share(ShareCmd),
    /// Delete the recorded events (the install id is kept)
    Clear,
}

#[derive(Args, Debug)]
pub struct SummaryCmd {
    /// Print the summary as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowCmd {
    /// How many events to print, newest last
    #[arg(short = 'n', long, default_value_t = 20)]
    pub count: usize,
}

#[derive(Args, Debug)]
pub struct ExportCmd {
    /// Output file
    pub path: std::path::PathBuf,
}

#[derive(Args, Debug)]
pub struct ShareCmd {
    /// on | off | now | status
    pub action: String,
    /// Endpoint to post batches to (with `on`)
    #[arg(long)]
    pub endpoint: Option<String>,
}

impl UsageCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            UsageSubcommand::Summary(cmd) => cmd.run(),
            UsageSubcommand::Show(cmd) => cmd.run(),
            UsageSubcommand::Export(cmd) => cmd.run(),
            UsageSubcommand::Status => status(),
            UsageSubcommand::On => set_record(true),
            UsageSubcommand::Off => set_record(false),
            UsageSubcommand::Share(cmd) => cmd.run(),
            UsageSubcommand::Clear => {
                usage::clear()?;
                eprintln!("Usage journal cleared.");
                Ok(())
            }
        }
    }
}

impl SummaryCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let events = usage::read_events();
        let summary = usage::summarize(&events);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        print!("{}", render_summary(&summary));
        Ok(())
    }
}

/// Human-readable summary. Pure so it can be tested.
pub fn render_summary(s: &usage::Summary) -> String {
    let mut out = String::new();
    if s.events == 0 {
        out.push_str("No usage recorded yet.\n");
        return out;
    }
    out.push_str(&format!(
        "{} events over {} active day{}\n",
        s.events,
        s.days_active,
        if s.days_active == 1 { "" } else { "s" }
    ));
    out.push_str(&format!(
        "machines {}  branches {}  sources {}  max depth {}\n",
        s.machines, s.branches, s.branch_sources, s.max_depth
    ));
    if let Some((min, median, max)) = s.fanout {
        out.push_str(&format!(
            "children per source  min {min}  median {median}  max {max}\n"
        ));
    }
    if !s.depth_histogram.is_empty() {
        let depths: Vec<String> = s
            .depth_histogram
            .iter()
            .map(|(depth, count)| format!("d{depth}:{count}"))
            .collect();
        out.push_str(&format!("branches by depth  {}\n", depths.join("  ")));
    }
    if let Some((min, median, max)) = s.lifetime_secs {
        out.push_str(&format!(
            "lifetime at delete  min {}  median {}  max {}\n",
            human_secs(min),
            human_secs(median),
            human_secs(max)
        ));
    }
    out.push_str(&format!(
        "cuda machines {}  networked machines {}  resizes {}  checkpoints {}  restores {}\n",
        s.cuda_machines, s.networked_machines, s.resizes, s.checkpoints, s.restores
    ));
    if !s.image_hosts.is_empty() {
        let hosts: Vec<String> = s
            .image_hosts
            .iter()
            .map(|(host, count)| format!("{host}:{count}"))
            .collect();
        out.push_str(&format!("image registries  {}\n", hosts.join("  ")));
    }
    let events: Vec<String> = s
        .by_event
        .iter()
        .map(|(event, count)| format!("{event}:{count}"))
        .collect();
    out.push_str(&format!("events  {}\n", events.join("  ")));
    out
}

fn human_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

impl ShowCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let events = usage::read_events();
        let start = events.len().saturating_sub(self.count);
        for event in &events[start..] {
            println!("{}", serde_json::to_string(event)?);
        }
        Ok(())
    }
}

impl ExportCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let events = usage::read_events();
        let mut out = String::new();
        for event in &events {
            out.push_str(&serde_json::to_string(event)?);
            out.push('\n');
        }
        std::fs::write(&self.path, out)?;
        eprintln!("Wrote {} events to {}", events.len(), self.path.display());
        Ok(())
    }
}

fn status() -> anyhow::Result<()> {
    let settings = SmolSettings::load()?;
    let events = usage::read_events();
    println!(
        "journal   {}",
        usage::journal_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(disabled: SMOLVM_USAGE_DIR is empty)".to_string())
    );
    println!("events    {}", events.len());
    println!(
        "record    {}{}",
        if settings.usage.record_enabled() {
            "on"
        } else {
            "off"
        },
        match std::env::var("SMOLVM_USAGE") {
            Ok(v) => format!("  (SMOLVM_USAGE={v} overrides)"),
            Err(_) => String::new(),
        }
    );
    println!(
        "share     {}  endpoint {}",
        if settings.usage.share_enabled() {
            "on"
        } else {
            "off"
        },
        settings.usage.endpoint()
    );
    println!("install   {}", usage::install_id());
    Ok(())
}

fn set_record(on: bool) -> anyhow::Result<()> {
    let mut settings = SmolSettings::load()?;
    settings.usage.record = Some(on);
    settings.save()?;
    eprintln!("Local usage recording {}.", if on { "on" } else { "off" });
    Ok(())
}

impl ShareCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.action.as_str() {
            "on" => {
                let mut settings = SmolSettings::load()?;
                settings.usage.share = Some(true);
                if let Some(endpoint) = self.endpoint {
                    settings.usage.endpoint = Some(endpoint);
                }
                settings.save()?;
                eprintln!(
                    "Usage sharing on. Events are posted to {} about once an hour when the CLI runs, or now with `smol usage share now`.",
                    settings.usage.endpoint()
                );
                Ok(())
            }
            "off" => {
                let mut settings = SmolSettings::load()?;
                settings.usage.share = Some(false);
                settings.save()?;
                eprintln!("Usage sharing off. The local journal is kept.");
                Ok(())
            }
            "now" => {
                let settings = SmolSettings::load()?;
                let report = usage::share_now(&settings.usage, std::time::Duration::from_secs(15))?;
                eprintln!("Sent {} events to {}.", report.sent, report.endpoint);
                Ok(())
            }
            "status" => status(),
            other => anyhow::bail!("unknown share action '{other}'. Use on, off, now or status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_summary_says_so() {
        let summary = usage::summarize(&[]);
        assert_eq!(render_summary(&summary), "No usage recorded yet.\n");
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_secs(5), "5s");
        assert_eq!(human_secs(600), "10m");
        assert_eq!(human_secs(7200), "2h");
        assert_eq!(human_secs(200_000), "2d");
    }
}
