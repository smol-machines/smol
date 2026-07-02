//! Unified machine reference resolver.
//!
//! A machine is one concept; its residency (`local` / `cloud`) is an attribute,
//! not a separate command tree. This module enumerates both backends and
//! resolves a user-supplied reference — optionally qualified `local/` or
//! `cloud/` — to exactly one machine, honoring an explicit `--local`/`--cloud`
//! target and the ambiguity rule (a bare name present in both backends must be
//! qualified).
//!
//! Cloud enumeration is **best-effort**: when the user is not logged in (or the
//! cloud endpoint is unset), cloud is silently skipped so that local-only
//! operations never fail or block on the network. See `machines.rs` for the
//! sync-resolve-creds-then-`block_on` split this mirrors — `cloud_client()`
//! performs a `block_on` token refresh internally and so must be called *before*
//! we enter a runtime.

use super::cloud;
use anyhow::{bail, Result};

/// Where a machine lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Local,
    Cloud,
}

impl Location {
    pub fn as_str(self) -> &'static str {
        match self {
            Location::Local => "local",
            Location::Cloud => "cloud",
        }
    }
}

/// Which backend(s) a command should consider, from `--local`/`--cloud` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Consider both backends; resolve by uniqueness (the default).
    Auto,
    Local,
    Cloud,
}

impl Target {
    /// Build a target from the two mutually-exclusive flags.
    pub fn from_flags(local: bool, cloud: bool) -> Result<Target> {
        match (local, cloud) {
            (true, true) => bail!("--local and --cloud are mutually exclusive"),
            (true, false) => Ok(Target::Local),
            (false, true) => Ok(Target::Cloud),
            (false, false) => Ok(Target::Auto),
        }
    }

    fn wants(self, loc: Location) -> bool {
        matches!(
            (self, loc),
            (Target::Auto, _) | (Target::Local, Location::Local) | (Target::Cloud, Location::Cloud)
        )
    }
}

/// A machine, unified across backends. `id` is the stable handle used by verbs
/// (the name for local machines, the `mach-…` id for cloud); `name` may be
/// absent for a half-created / pool-vended cloud row.
#[derive(Debug, Clone)]
pub struct MachineRef {
    pub location: Location,
    pub name: Option<String>,
    pub id: String,
    pub state: String,
    /// Human-facing source (image/reference), best-effort, for listings.
    pub source: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u32>,
}

impl MachineRef {
    /// The display handle: prefer the name, fall back to the id.
    #[allow(dead_code)] // used by verbs wired in follow-ups (see docs/unified-machine-cli.md)
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// True if a bare user reference selects this machine.
    fn matches_bare(&self, reference: &str) -> bool {
        self.id == reference || self.name.as_deref() == Some(reference)
    }
}

/// A parsed reference: an optional forced location plus the bare name-or-id.
struct ParsedRef<'a> {
    forced: Option<Location>,
    bare: &'a str,
}

/// Parse `local/foo` / `cloud/foo` / `foo` / `mach-…`. An id beginning with
/// `mach-` is unambiguously cloud even without a prefix.
fn parse_ref(reference: &str) -> ParsedRef<'_> {
    if let Some(rest) = reference.strip_prefix("local/") {
        return ParsedRef { forced: Some(Location::Local), bare: rest };
    }
    if let Some(rest) = reference.strip_prefix("cloud/") {
        return ParsedRef { forced: Some(Location::Cloud), bare: rest };
    }
    if reference.starts_with("mach-") {
        return ParsedRef { forced: Some(Location::Cloud), bare: reference };
    }
    ParsedRef { forced: None, bare: reference }
}

/// Enumerate local machines from the on-disk config. Synchronous, never fails
/// on a missing cloud login.
fn list_local() -> Result<Vec<MachineRef>> {
    let config = smolvm::config::SmolvmConfig::load()?;
    Ok(config
        .list_vms()
        .map(|(name, record)| MachineRef {
            location: Location::Local,
            name: Some(name.clone()),
            id: name.clone(),
            state: record.actual_state().to_string(),
            source: record.image.clone(),
            cpus: Some(record.cpus as u32),
            memory_mib: Some(record.mem),
        })
        .collect())
}

/// Enumerate cloud machines, best-effort. Returns `Ok(None)` (not an error) when
/// the user is not logged in / the endpoint is unset, so callers can degrade to
/// local-only cleanly. Real network/auth failures after a valid login DO surface.
fn list_cloud() -> Result<Option<Vec<MachineRef>>> {
    // Not logged in → skip the cloud entirely rather than fire an unauthenticated
    // request that would 401 and hard-error. This is the fresh-install and
    // post-`smol logout` default, and the whole point of best-effort enumeration.
    if !cloud::cloud_is_authenticated() {
        return Ok(None);
    }
    // `cloud_client()` can still fail cleanly (expired token + refresh failed);
    // for a best-effort listing treat that as "cloud unavailable", not a hard
    // error. It also does a `block_on` refresh internally, so it must run before
    // we build our own runtime.
    let (http, cloud_config) = match cloud::cloud_client() {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let endpoint = cloud_config.endpoint()?.to_string();

    let rt = tokio::runtime::Runtime::new()?;
    // Unreachable control plane (offline) or rejected credentials (401/403) →
    // degrade to local; only genuine server/protocol errors surface here.
    let machines = match rt.block_on(cloud::list_machines_best_effort(&http, &endpoint))? {
        Some(machines) => machines,
        None => return Ok(None),
    };
    Ok(Some(
        machines
            .into_iter()
            .map(|m| MachineRef {
                location: Location::Cloud,
                name: m.name.clone(),
                id: m.id,
                state: m.state,
                source: m.source.and_then(|s| s.reference),
                cpus: m.resources.as_ref().and_then(|r| r.cpus),
                memory_mib: m.resources.and_then(|r| r.memory_mb),
            })
            .collect(),
    ))
}

/// Result of a unified enumeration: the machines plus whether cloud was reachable
/// (so listings can print a "cloud: not logged in" hint without it being an error).
pub struct Listing {
    pub machines: Vec<MachineRef>,
    pub cloud_available: bool,
}

/// Enumerate all machines the target selects. Cloud is best-effort under `Auto`
/// and `Cloud`; under `Local` the cloud is not contacted at all.
pub fn list_all(target: Target) -> Result<Listing> {
    let mut machines = Vec::new();
    let mut cloud_available = true;

    if target.wants(Location::Local) {
        machines.extend(list_local()?);
    }
    if target.wants(Location::Cloud) {
        match list_cloud()? {
            Some(cloud) => machines.extend(cloud),
            None => cloud_available = false,
        }
    } else {
        // Local-only target: cloud availability is irrelevant, don't imply it.
        cloud_available = false;
    }

    Ok(Listing { machines, cloud_available })
}

/// Resolve a user reference to exactly one machine under `target`.
///
/// Errors, with actionable text, when the reference matches nothing or is
/// ambiguous (a bare name present in both backends). A `local/`/`cloud/` prefix
/// or a `mach-…` id removes the ambiguity, as does a `--local`/`--cloud` flag.
///
/// Not yet called: this is the wiring target for the remaining verbs (exec,
/// shell, start, stop, rm, logs, status, cp, fork) per `docs/unified-machine-cli.md`.
#[allow(dead_code)]
pub fn resolve(reference: &str, target: Target) -> Result<MachineRef> {
    let parsed = parse_ref(reference);

    // A forced location (prefix or mach- id) narrows the effective target, and
    // must not conflict with an explicit flag.
    let effective = match (parsed.forced, target) {
        (Some(Location::Local), Target::Cloud) | (Some(Location::Cloud), Target::Local) => {
            bail!(
                "reference '{reference}' forces a location that conflicts with the \
                 --local/--cloud flag"
            )
        }
        (Some(loc), _) => match loc {
            Location::Local => Target::Local,
            Location::Cloud => Target::Cloud,
        },
        (None, t) => t,
    };

    let listing = list_all(effective)?;
    let mut hits: Vec<MachineRef> = listing
        .machines
        .into_iter()
        .filter(|m| m.matches_bare(parsed.bare))
        .collect();

    match hits.len() {
        0 => {
            if effective != Target::Local && !listing.cloud_available {
                bail!(
                    "machine '{}' not found locally; cloud was not searched \
                     (not logged in — run 'smol auth login')",
                    parsed.bare
                );
            }
            bail!("machine '{}' not found", parsed.bare)
        }
        1 => Ok(hits.pop().unwrap()),
        _ => {
            // Ambiguous only when the hits straddle both backends; qualify.
            let locals = hits.iter().filter(|m| m.location == Location::Local).count();
            let clouds = hits.len() - locals;
            if locals > 0 && clouds > 0 {
                bail!(
                    "'{name}' is ambiguous — it exists both locally and in the cloud. \
                     Qualify it as 'local/{name}' or 'cloud/{name}'.",
                    name = parsed.bare
                )
            }
            // Same-backend duplicate (e.g. two cloud rows share a name): prefer id
            // match if the reference was an id, else it's genuinely ambiguous.
            if let Some(exact) = hits.iter().find(|m| m.id == parsed.bare) {
                return Ok(exact.clone());
            }
            bail!(
                "'{}' matches {} machines in the same backend; use the machine id to \
                 disambiguate",
                parsed.bare,
                hits.len()
            )
        }
    }
}

/// Does a local machine with this name exist? Cheap, offline registry lookup.
fn local_exists(name: &str) -> Result<bool> {
    let config = smolvm::config::SmolvmConfig::load()?;
    let exists = config.list_vms().any(|(n, _)| n == name);
    Ok(exists)
}

/// Does a cloud machine with this name/id exist? Best-effort: `false` (not an
/// error) when not logged in, so auto-resolution never forces a login.
fn cloud_has(name: &str) -> Result<bool> {
    match list_cloud()? {
        Some(ms) => Ok(ms
            .iter()
            .any(|m| m.id == name || m.name.as_deref() == Some(name))),
        None => Ok(false),
    }
}

/// Reconcile a reference's forced location (a `local/`/`cloud/` prefix or a
/// `mach-…` id) with an explicit `--local`/`--cloud` target, returning the
/// resulting location *hint* (`None` = decide by lookup) and the bare handle
/// (prefix stripped). Errors when the prefix/id and the flag contradict.
///
/// This is the IO-free front half shared by `locate` and `route`; the two
/// differ only in what they do with a bare name that matches nothing.
fn hint_and_bare(
    reference: Option<&str>,
    target: Target,
) -> Result<(Option<Location>, Option<String>)> {
    let parsed = reference.map(parse_ref);
    let forced = parsed.as_ref().and_then(|p| p.forced);
    let bare = parsed.as_ref().map(|p| p.bare.to_string());

    // A prefix/id-implied location must not contradict an explicit flag.
    match (forced, target) {
        (Some(Location::Local), Target::Cloud) | (Some(Location::Cloud), Target::Local) => {
            bail!("reference forces a location that conflicts with the --local/--cloud flag")
        }
        _ => {}
    }

    let hint = forced.or(match target {
        Target::Local => Some(Location::Local),
        Target::Cloud => Some(Location::Cloud),
        Target::Auto => None,
    });
    Ok((hint, bare))
}

/// Decide where a verb should act, returning `(location, bare_handle)`.
///
/// Policy — an explicit `--local`/`--cloud` flag or a `local/`/`cloud/` prefix
/// (or a `mach-…` id) wins with no enumeration. A bare name under `Auto` is
/// resolved **local-first** (offline registry hit → local; historical default),
/// then cloud as a fallback, so existing local execs keep working with no
/// network and no login while a cloud-only name still resolves with no flag. A
/// missing reference means the local `default` machine, exactly as before.
///
/// A bare name that matches **nothing** is an error here: this is the policy for
/// verbs that require an existing machine (`exec`, `shell`). Lifecycle verbs use
/// [`route`] instead, which falls back to local so their own messaging speaks.
pub fn locate(reference: Option<&str>, target: Target) -> Result<(Location, String)> {
    let (hint, bare) = hint_and_bare(reference, target)?;

    match hint {
        Some(Location::Cloud) => {
            let name = bare.ok_or_else(|| {
                anyhow::anyhow!("machine name or ID required for a cloud machine")
            })?;
            Ok((Location::Cloud, name))
        }
        // Local (or forced-local) needs no existence check here — the caller's
        // connect path reports a missing machine, matching prior behavior.
        Some(Location::Local) => Ok((Location::Local, bare.unwrap_or_else(|| "default".into()))),
        None => match bare {
            // No name → the local `default` machine, offline, as before.
            None => Ok((Location::Local, "default".into())),
            Some(name) => {
                if local_exists(&name)? {
                    Ok((Location::Local, name))
                } else if cloud_has(&name)? {
                    Ok((Location::Cloud, name))
                } else {
                    bail!(
                        "machine '{name}' not found locally or in the cloud \
                         (if it is a cloud machine, run 'smol auth login')"
                    )
                }
            }
        },
    }
}

/// Like [`locate`], but a bare name that matches nothing falls back to **local**
/// instead of erroring.
///
/// This is the policy for lifecycle verbs (`start`, `stop`, `rm`, `status`,
/// `logs`, `fork`, `cp`): each owns its local not-found / create-first /
/// bare-`default` messaging, which is more specific than a generic "not found"
/// here. A name that is *exclusively* a cloud machine still routes to the cloud
/// with no flag; an unknown name is handed to the local path to report in its
/// own terms (and never forces a cloud login).
pub fn route(reference: Option<&str>, target: Target) -> Result<(Location, String)> {
    let (hint, bare) = hint_and_bare(reference, target)?;

    match hint {
        Some(Location::Cloud) => {
            let name = bare.ok_or_else(|| {
                anyhow::anyhow!("machine name or ID required for a cloud machine")
            })?;
            Ok((Location::Cloud, name))
        }
        Some(Location::Local) => Ok((Location::Local, bare.unwrap_or_else(|| "default".into()))),
        None => match bare {
            None => Ok((Location::Local, "default".into())),
            Some(name) => {
                if local_exists(&name)? {
                    Ok((Location::Local, name))
                } else if cloud_has(&name)? {
                    Ok((Location::Cloud, name))
                } else {
                    // Unknown to both (or logged out): let the local verb report
                    // create-first / not-found in its own, more specific terms.
                    Ok((Location::Local, name))
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(loc: Location, name: &str, id: &str) -> MachineRef {
        MachineRef {
            location: loc,
            name: Some(name.to_string()),
            id: id.to_string(),
            state: "running".into(),
            source: None,
            cpus: None,
            memory_mib: None,
        }
    }

    #[test]
    fn parses_location_prefixes() {
        assert_eq!(parse_ref("local/foo").forced, Some(Location::Local));
        assert_eq!(parse_ref("cloud/foo").forced, Some(Location::Cloud));
        assert_eq!(parse_ref("foo").forced, None);
        // A mach- id is unambiguously cloud without a prefix.
        assert_eq!(parse_ref("mach-abc123").forced, Some(Location::Cloud));
        assert_eq!(parse_ref("local/foo").bare, "foo");
    }

    #[test]
    fn target_from_flags_rejects_both() {
        assert!(Target::from_flags(true, true).is_err());
        assert_eq!(Target::from_flags(true, false).unwrap(), Target::Local);
        assert_eq!(Target::from_flags(false, true).unwrap(), Target::Cloud);
        assert_eq!(Target::from_flags(false, false).unwrap(), Target::Auto);
    }

    #[test]
    fn bare_match_prefers_id_or_name() {
        let cloud = m(Location::Cloud, "codex-box", "mach-b0bc");
        assert!(cloud.matches_bare("codex-box"));
        assert!(cloud.matches_bare("mach-b0bc"));
        assert!(!cloud.matches_bare("other"));
    }

    #[test]
    fn locate_honors_flags_and_prefixes_without_io() {
        // Explicit --cloud → cloud, name preserved.
        assert_eq!(
            locate(Some("codex-box"), Target::Cloud).unwrap(),
            (Location::Cloud, "codex-box".to_string())
        );
        // Explicit --local → local.
        assert_eq!(
            locate(Some("myvm"), Target::Local).unwrap(),
            (Location::Local, "myvm".to_string())
        );
        // cloud/ prefix under auto → cloud, prefix stripped.
        assert_eq!(
            locate(Some("cloud/codex-box"), Target::Auto).unwrap(),
            (Location::Cloud, "codex-box".to_string())
        );
        // local/ prefix under auto → local, prefix stripped.
        assert_eq!(
            locate(Some("local/myvm"), Target::Auto).unwrap(),
            (Location::Local, "myvm".to_string())
        );
        // mach- id under auto → cloud without a prefix.
        assert_eq!(
            locate(Some("mach-abc123"), Target::Auto).unwrap(),
            (Location::Cloud, "mach-abc123".to_string())
        );
        // No reference → local default, offline.
        assert_eq!(locate(None, Target::Auto).unwrap(), (Location::Local, "default".to_string()));
    }

    #[test]
    fn locate_rejects_prefix_flag_conflict() {
        assert!(locate(Some("cloud/foo"), Target::Local).is_err());
        assert!(locate(Some("local/foo"), Target::Cloud).is_err());
    }

    #[test]
    fn route_honors_flags_and_prefixes_without_io() {
        // Same fast paths as `locate` — flags/prefixes/id decide with no lookup.
        assert_eq!(
            route(Some("codex-box"), Target::Cloud).unwrap(),
            (Location::Cloud, "codex-box".to_string())
        );
        assert_eq!(
            route(Some("myvm"), Target::Local).unwrap(),
            (Location::Local, "myvm".to_string())
        );
        assert_eq!(
            route(Some("cloud/codex-box"), Target::Auto).unwrap(),
            (Location::Cloud, "codex-box".to_string())
        );
        assert_eq!(
            route(Some("local/myvm"), Target::Auto).unwrap(),
            (Location::Local, "myvm".to_string())
        );
        assert_eq!(
            route(Some("mach-abc123"), Target::Auto).unwrap(),
            (Location::Cloud, "mach-abc123".to_string())
        );
        // No reference → local default, offline.
        assert_eq!(route(None, Target::Auto).unwrap(), (Location::Local, "default".to_string()));
    }

    #[test]
    fn route_rejects_prefix_flag_conflict() {
        assert!(route(Some("cloud/foo"), Target::Local).is_err());
        assert!(route(Some("local/foo"), Target::Cloud).is_err());
    }

    #[test]
    fn display_name_falls_back_to_id() {
        let unnamed = MachineRef {
            location: Location::Cloud,
            name: None,
            id: "mach-5bb1".into(),
            state: "stopped".into(),
            source: None,
            cpus: None,
            memory_mib: None,
        };
        assert_eq!(unnamed.display_name(), "mach-5bb1");
    }
}
