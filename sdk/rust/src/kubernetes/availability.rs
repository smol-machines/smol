//! Read-only comparison of local image tags with the cluster's CRI inventory.
use super::{check_cancel, failure, feed, Kubernetes, BIN};
use crate::{ErrorKind, MachineState, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Automatic import configuration for this cluster.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalImageMode {
    /// The host mount and guest importer are configured for automatic images.
    Automatic,
    /// Stop and start the cluster to attach or configure automatic imports.
    RestartRequired,
    /// Compute is stopped; the guest inventory has not been queried.
    Stopped,
    /// Compute is paused; the guest inventory has not been queried.
    Paused,
    /// Compute is not available for inventory inspection.
    Unavailable,
}

/// Availability of this exact host tag and image digest in the selected cluster.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalImageState {
    /// The guest resolves this tag to the current host image digest.
    Ready,
    /// A host archive is published but the guest tag has not caught up.
    Importing,
    /// No host archive is published for this tag.
    NotShared,
    /// Automatic import configuration needs a stop and start.
    RestartRequired,
    /// The guest is stopped, so availability is unknown.
    Stopped,
    /// The guest is paused, so availability is unknown.
    Paused,
    /// The guest is unavailable, so availability is unknown.
    Unavailable,
}

/// One host image tag and its observed availability in the selected cluster.
#[derive(Debug, Serialize)]
pub struct LocalImage {
    /// Host image configuration digest.
    pub id: String,
    /// Host tag, preserving its original spelling.
    pub tag: String,
    /// Availability at the time this snapshot was read.
    pub state: LocalImageState,
}

/// A snapshot; ready means the guest currently resolves the tag to this digest.
#[derive(Debug, Serialize)]
pub struct LocalImages {
    /// Automatic import configuration or compute state.
    pub mode: LocalImageMode,
    /// Tagged host images, sorted by tag and digest.
    pub images: Vec<LocalImage>,
}

#[derive(Deserialize)]
struct HostFeed {
    attached: bool,
    images: Vec<HostImage>,
}
#[derive(Deserialize)]
struct HostImage {
    id: String,
    tags: Vec<String>,
    published: bool,
}
#[derive(Deserialize)]
struct Inventory {
    images: Vec<GuestImage>,
}
#[derive(Deserialize)]
struct GuestImage {
    id: String,
    #[serde(rename = "repoTags", default)]
    tags: Vec<String>,
}

impl Kubernetes {
    /// Inspect local tag availability without starting compute, publishing
    /// archives, changing mounts or importing images.
    pub fn local_images(&self, cancelled: &AtomicBool) -> Result<LocalImages> {
        self.owned()?;
        check_cancel(cancelled)?;
        let bytes = feed::engine(
            &[
                "machine".into(),
                "image-feed".into(),
                "--status".into(),
                "--machine".into(),
                self.machine.name().into(),
            ],
            Duration::from_secs(10),
            cancelled,
        )?;
        let host: HostFeed = serde_json::from_slice(&bytes).map_err(|error| {
            failure(
                ErrorKind::Other,
                format!("Read local image catalog: {error}"),
            )
        })?;
        let (mode, guest) = match self.machine.state() {
            MachineState::Running | MachineState::Started => {
                check_cancel(cancelled)?;
                let result = self.exec([
                    "sh".into(), "-c".into(),
                    format!("set -eu\nif test -d /run/smolvm/image-feed && test -e /proc/smolvm-fsnotify && test \"$(readlink /workspace/.smol/kubernetes/data/agent/images/smol)\" = /run/smolvm/image-feed; then echo automatic; else echo restart_required; fi\nexec {BIN} crictl images -o json"),
                ], Duration::from_secs(10))?;
                if !result.success() {
                    return Err(failure(
                        ErrorKind::CommandFailed,
                        format!("Read cluster images: {}", result.stderr_utf8().trim()),
                    ));
                }
                let output = result.stdout_utf8();
                let (configured, json) = output.split_once('\n').ok_or_else(|| {
                    failure(ErrorKind::Other, "Cluster returned no image inventory")
                })?;
                let inventory: Inventory = serde_json::from_str(json).map_err(|error| {
                    failure(
                        ErrorKind::Other,
                        format!("Read cluster image inventory: {error}"),
                    )
                })?;
                let mode = if host.attached && configured == "automatic" {
                    LocalImageMode::Automatic
                } else {
                    LocalImageMode::RestartRequired
                };
                (mode, inventory.images)
            }
            MachineState::Stopped => (LocalImageMode::Stopped, Vec::new()),
            MachineState::Paused => (LocalImageMode::Paused, Vec::new()),
            _ => (LocalImageMode::Unavailable, Vec::new()),
        };
        check_cancel(cancelled)?;
        Ok(compare(host.images, guest, mode))
    }
}

fn compare(host: Vec<HostImage>, guest: Vec<GuestImage>, mode: LocalImageMode) -> LocalImages {
    let mut tags = HashMap::new();
    for image in guest {
        for tag in image.tags {
            tags.insert(canonical_tag(&tag), image.id.clone());
        }
    }
    let mut images = Vec::new();
    for image in host {
        for tag in image.tags {
            let state = if tags.get(&canonical_tag(&tag)) == Some(&image.id) {
                LocalImageState::Ready
            } else {
                match mode {
                    LocalImageMode::Stopped => LocalImageState::Stopped,
                    LocalImageMode::Paused => LocalImageState::Paused,
                    LocalImageMode::Unavailable => LocalImageState::Unavailable,
                    LocalImageMode::RestartRequired => LocalImageState::RestartRequired,
                    LocalImageMode::Automatic if image.published => LocalImageState::Importing,
                    LocalImageMode::Automatic => LocalImageState::NotShared,
                }
            };
            images.push(LocalImage {
                id: image.id.clone(),
                tag,
                state,
            });
        }
    }
    images.sort_by(|left, right| left.tag.cmp(&right.tag).then(left.id.cmp(&right.id)));
    LocalImages { mode, images }
}

fn canonical_tag(tag: &str) -> String {
    let first = tag.split('/').next().unwrap_or_default();
    let mut tag = if tag.contains('/') && (first.contains(['.', ':']) || first == "localhost") {
        tag.strip_prefix("index.docker.io/")
            .map(|rest| format!("docker.io/{rest}"))
            .unwrap_or_else(|| tag.to_owned())
    } else {
        format!("docker.io/{tag}")
    };
    if let Some(rest) = tag.strip_prefix("docker.io/") {
        if !rest.contains('/') {
            tag = format!("docker.io/library/{rest}");
        }
    }
    if !tag.rsplit('/').next().unwrap_or_default().contains(':') {
        tag.push_str(":latest");
    }
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuild_is_not_ready_until_the_guest_tag_resolves_to_the_new_digest() {
        let host = vec![HostImage {
            id: "sha256:new".into(),
            tags: vec!["app:dev".into(), "team/app:dev".into()],
            published: true,
        }];
        let guest = vec![
            GuestImage {
                id: "sha256:old".into(),
                tags: vec!["docker.io/library/app:dev".into()],
            },
            GuestImage {
                id: "sha256:new".into(),
                tags: vec!["docker.io/team/app:dev".into()],
            },
        ];
        let result = compare(host, guest, LocalImageMode::Automatic);
        assert_eq!(result.images[0].state, LocalImageState::Importing);
        assert_eq!(result.images[1].state, LocalImageState::Ready);
    }

    #[test]
    fn manual_imports_can_be_ready_without_an_archive_or_automatic_mount() {
        let host = vec![HostImage {
            id: "sha256:same".into(),
            tags: vec!["localhost:5000/app:dev".into()],
            published: false,
        }];
        let guest = vec![GuestImage {
            id: "sha256:same".into(),
            tags: vec!["localhost:5000/app:dev".into()],
        }];
        assert_eq!(
            compare(host, guest, LocalImageMode::RestartRequired).images[0].state,
            LocalImageState::Ready
        );
        assert_eq!(
            canonical_tag("index.docker.io/alpine"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            canonical_tag("quay.io/team/app:dev"),
            "quay.io/team/app:dev"
        );
    }
}
