//! `smol auth status` — show who you're authenticated as and what you can do.
//!
//! Works for both credential types smol accepts:
//! - an Auth0 **user session** from `smol auth login` (a JWT), and
//! - an opaque **API key** (`smk_…`) created in the console for CI/SDK use.
//!
//! The control plane's `GET /v1/me` is the source of truth for identity and
//! access — the only way to resolve scopes for an opaque API key — so status
//! queries it and prints the tenant, scopes, and the registry namespace the
//! caller can push to.

use anyhow::Result;
use clap::Args;
use serde::{Deserialize, Serialize};
use smolvm::SmolSettings;

#[derive(Args, Debug)]
pub struct AuthStatusCmd {
    /// Output machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// The control plane's `GET /v1/me` view of the caller.
#[derive(Debug, Deserialize, Serialize)]
struct Me {
    #[serde(default)]
    subject: Option<String>,
    #[serde(rename = "tenantId", default)]
    tenant_id: Option<String>,
    #[serde(rename = "tenantName", default)]
    tenant_name: Option<String>,
    #[serde(rename = "tenantStatus", default)]
    tenant_status: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(rename = "registryNamespace", default)]
    registry_namespace: Option<String>,
}

enum FetchErr {
    /// The credential was rejected (401) — expired or revoked.
    Unauthorized,
    /// Anything else (offline, 5xx, parse) — carries a message.
    Other(String),
}

impl AuthStatusCmd {
    pub fn run(self) -> Result<()> {
        let settings = SmolSettings::load()?;
        let cloud = &settings.cloud;
        let key = cloud.api_key.as_deref().filter(|k| !k.is_empty());
        let endpoint = cloud
            .endpoint()
            .unwrap_or(smolvm::registry::SMOLMACHINES_API)
            .to_string();

        let Some(key) = key else {
            if self.json {
                println!("{}", serde_json::json!({ "loggedIn": false }));
            } else {
                println!("Not logged in.");
                println!();
                println!("  Log in with your account:  smol auth login");
                println!("  Or configure an API key:   smol config set cloud.api_key smk_…");
            }
            return Ok(());
        };

        // API keys are opaque (`smk_…`); a user session is an Auth0 JWT.
        let is_api_key = key.starts_with("smk_");
        let kind = if is_api_key { "API key" } else { "user session" };

        match fetch_me(&endpoint, key) {
            Ok(me) => {
                if self.json {
                    let out = serde_json::json!({
                        "loggedIn": true,
                        "endpoint": endpoint,
                        "credentialType": if is_api_key { "api_key" } else { "user" },
                        "me": me,
                    });
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    print_human(&me, kind, &endpoint, cloud);
                }
            }
            Err(FetchErr::Unauthorized) => {
                if self.json {
                    println!(
                        "{}",
                        serde_json::json!({ "loggedIn": false, "error": "unauthorized" })
                    );
                } else {
                    println!("Credential rejected (401) — it may be expired or revoked.");
                    if is_api_key {
                        println!("  Check the key, or create a new one in the console.");
                    } else {
                        println!("  Run `smol auth login` to re-authenticate.");
                    }
                }
                std::process::exit(1);
            }
            Err(FetchErr::Other(e)) => {
                // Offline / control-plane unreachable: report what we know locally.
                if self.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "loggedIn": true,
                            "credentialType": if is_api_key { "api_key" } else { "user" },
                            "endpoint": endpoint,
                            "reachable": false,
                            "error": e,
                        })
                    );
                } else {
                    println!("Logged in ({kind}), but couldn't reach {endpoint}:");
                    println!("  {e}");
                    if !is_api_key {
                        if let Some(exp) = cloud.token_expires_at {
                            println!("  Session {}.", expiry_phrase(exp));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Query `GET {endpoint}/v1/me` with the bearer credential.
fn fetch_me(endpoint: &str, key: &str) -> std::result::Result<Me, FetchErr> {
    let url = format!("{}/v1/me", endpoint.trim_end_matches('/'));
    let rt = tokio::runtime::Runtime::new().map_err(|e| FetchErr::Other(e.to_string()))?;
    rt.block_on(async {
        let client = super::common::http_client().map_err(|e| FetchErr::Other(e.to_string()))?;
        let resp = client
            .get(&url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| FetchErr::Other(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FetchErr::Unauthorized);
        }
        if !resp.status().is_success() {
            return Err(FetchErr::Other(format!("HTTP {}", resp.status())));
        }
        resp.json::<Me>()
            .await
            .map_err(|e| FetchErr::Other(e.to_string()))
    })
}

fn print_human(me: &Me, kind: &str, endpoint: &str, cloud: &smolvm::settings::CloudSection) {
    println!("Logged in ✓");
    println!();
    if let Some(s) = &me.subject {
        println!("  Subject    {s}  ({kind})");
    }
    let name = me.tenant_name.as_deref().unwrap_or("—");
    match (&me.tenant_id, &me.tenant_status) {
        (Some(id), Some(status)) => println!("  Tenant     {name}  ({id}, {status})"),
        (Some(id), None) => println!("  Tenant     {name}  ({id})"),
        _ => println!("  Tenant     {name}"),
    }
    match me.scopes.as_deref() {
        Some(scopes) if !scopes.is_empty() => println!("  Access     {}", scopes.join(", ")),
        _ => println!("  Access     (no scopes — this credential can't do anything)"),
    }
    // The namespace the caller may push artifacts to — answers "where do I push?".
    if let Some(ns) = &me.registry_namespace {
        println!(
            "  Registry   {}/{ns}",
            smolvm::registry::SMOLMACHINES_REGISTRY
        );
    }
    println!("  Endpoint   {endpoint}");
    // Session expiry only applies to a user JWT; API keys are managed in the console.
    if kind == "user session" {
        if let Some(exp) = cloud.token_expires_at {
            println!("  Session    {}", expiry_phrase(exp));
        }
    }
}

/// A human phrase for a Unix-timestamp expiry.
fn expiry_phrase(exp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if now >= exp {
        return "expired — run `smol auth login`".to_string();
    }
    let secs = exp - now;
    let hrs = secs / 3600;
    if hrs >= 48 {
        format!("valid for {} days", hrs / 24)
    } else if hrs >= 1 {
        format!("valid for {hrs}h")
    } else {
        format!("valid for {}m", (secs / 60).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn me_deserializes_real_api_key_response() {
        // The exact shape the control plane returns for an API-key caller.
        let json = r#"{
            "subject": "key-EXAMPLEsubjectid",
            "tenantId": "tenant-EXAMPLEid",
            "tenantName": "org_bDLcJYfR4JcAkZLB",
            "tenantStatus": "active",
            "scopes": ["machine:create","machine:read","machine:exec","machine:delete","machine:files"],
            "registryNamespace": "tenants/tenant-EXAMPLEid"
        }"#;
        let me: Me = serde_json::from_str(json).unwrap();
        assert_eq!(me.subject.as_deref(), Some("key-EXAMPLEsubjectid"));
        assert_eq!(me.tenant_name.as_deref(), Some("org_bDLcJYfR4JcAkZLB"));
        assert_eq!(me.tenant_status.as_deref(), Some("active"));
        assert_eq!(me.scopes.as_ref().unwrap().len(), 5);
        assert_eq!(
            me.registry_namespace.as_deref(),
            Some("tenants/tenant-EXAMPLEid")
        );
    }

    #[test]
    fn me_tolerates_missing_optional_fields() {
        let me: Me = serde_json::from_str(r#"{"subject":"user|1"}"#).unwrap();
        assert_eq!(me.subject.as_deref(), Some("user|1"));
        assert!(me.scopes.is_none());
        assert!(me.registry_namespace.is_none());
    }

    #[test]
    fn expiry_phrase_reports_expired_for_past_timestamps() {
        assert_eq!(expiry_phrase(0), "expired — run `smol auth login`");
    }
}
