//! Configuration loaded from the process environment (`wasi:cli/environment`)
//! plus a preopened state directory (`wasi:filesystem/preopens`).
//!
//! Nothing sensitive is hard-coded here: the OAuth `client_id`/`tenant_id`
//! are supplied by whoever registers the Azure AD app (documented in the
//! top-level README), and the refresh token itself lives only in
//! `{state_dir}/token.json`, never in an env var or in this struct.

use crate::bindings::wasi::cli::environment;
use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::Descriptor;

pub struct Config {
    pub client_id: Option<String>,
    pub tenant_id: String,
    pub drive_base: String,
    pub auth_scopes: String,
    pub basic_auth_secret: Option<String>,
    /// Preopened directory used to persist `token.json`. Must be granted to
    /// the component by the native Wasmtime host.
    pub state_dir: Descriptor,
    /// Serve PROPFIND/HEAD from the local snapshot. `ONEDRIVE_INDEX_ENABLED=0`
    /// restores live Graph listings.
    pub index_enabled: bool,
    pub sync_max_pages: usize,
    pub sync_budget_ms: u64,
}

pub const DEFAULT_AUTH_SCOPES: &str = "offline_access Files.ReadWrite.All User.Read";

fn env(name: &str) -> Option<String> {
    environment::get_environment()
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

impl Config {
    pub fn cache_current(&self) -> bool {
        if !self.index_enabled {
            return false;
        }
        let Ok(Some(meta)) = crate::snapshot::read_meta(&self.state_dir) else {
            return false;
        };
        let Ok(credential_id) = crate::auth::credential_id(self) else {
            return false;
        };
        meta.binding.is_some_and(|binding| {
            binding.matches_configuration(
                &self.tenant_id,
                self.client_id.as_deref(),
                &self.drive_base,
                credential_id.as_deref(),
            )
        })
    }

    /// Loads configuration from the environment and preopens. A client id
    /// is optional at startup so a pre-seeded, still-valid access token can
    /// be used immediately; only token refresh requires `ONEDRIVE_CLIENT_ID`.
    pub fn load() -> Result<Self, String> {
        let client_id = env("ONEDRIVE_CLIENT_ID");
        let tenant_id = env("ONEDRIVE_TENANT_ID").unwrap_or_else(|| "common".to_string());
        validate_authority(&tenant_id)?;
        let drive_base = normalize_drive_base(
            &env("ONEDRIVE_DRIVE_BASE").unwrap_or_else(|| "me/drive".to_string()),
        )?;
        let auth_scopes = normalize_auth_scopes(
            env("ONEDRIVE_SCOPES")
                .as_deref()
                .unwrap_or(DEFAULT_AUTH_SCOPES),
        )?;
        let basic_auth_secret = validate_basic_auth_secret(
            env("ONEDRIVE_BASIC_AUTH_SECRET"),
            env("ONEDRIVE_ALLOW_UNAUTHENTICATED").as_deref() == Some("1"),
        )?;
        let index_enabled = match env("ONEDRIVE_INDEX_ENABLED").as_deref() {
            Some("0") | Some("false") | Some("FALSE") => false,
            _ => true,
        };
        let sync_max_pages = env("ONEDRIVE_SYNC_MAX_PAGES")
            .and_then(|s| s.parse().ok())
            .unwrap_or(8)
            .max(1);
        let sync_budget_ms = env("ONEDRIVE_SYNC_BUDGET_MS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000)
            .max(1);

        let mut state_dir = None;
        for (descriptor, path) in preopens::get_directories() {
            if path == "/state" {
                state_dir = Some(descriptor);
                break;
            }
        }
        let state_dir = state_dir
            .ok_or("no /state preopen found; run wasmtime with --dir <state-dir>::/state")?;

        Ok(Config {
            client_id,
            tenant_id,
            drive_base,
            auth_scopes,
            basic_auth_secret,
            state_dir,
            index_enabled,
            sync_max_pages,
            sync_budget_ms,
        })
    }
}

fn normalize_auth_scopes(raw: &str) -> Result<String, String> {
    let scopes = raw
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return Err("ONEDRIVE_SCOPES must contain at least one OAuth scope".to_string());
    }
    if !scopes.contains(&"offline_access")
        || scopes.iter().any(|scope| {
            !matches!(
                *scope,
                "offline_access" | "Files.ReadWrite.All" | "User.Read" | "Sites.Read.All"
            )
        })
    {
        return Err(
            "ONEDRIVE_SCOPES must include offline_access and only supported delegated Graph scopes"
                .into(),
        );
    }
    Ok(scopes.join(" "))
}

pub fn normalize_drive_base(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.eq_ignore_ascii_case("me/drive") {
        return Ok("me/drive".to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("drives/") {
        validate_selector_id(rest, "drive id")?;
        return Ok(format!("drives/{rest}"));
    }
    for prefix in ["users", "groups", "sites"] {
        let head = format!("{prefix}/");
        if let Some(rest) = trimmed.strip_prefix(&head) {
            let Some(id) = rest.strip_suffix("/drive") else {
                return Err(format!(
                    "unsupported ONEDRIVE_DRIVE_BASE selector {trimmed:?}; expected {prefix}/<id>/drive"
                ));
            };
            validate_selector_id(id, &format!("{prefix} selector id"))?;
            return Ok(format!("{prefix}/{id}/drive"));
        }
    }
    validate_selector_id(trimmed, "drive id")?;
    Ok(format!("drives/{trimmed}"))
}

pub(crate) fn validate_selector_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(format!("{label} must not be empty"));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(format!("{label} must not contain path separators"));
    }
    if value
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '!' | '-' | '_' | '.' | '@' | ','))
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(())
}

fn validate_authority(tenant: &str) -> Result<(), String> {
    if tenant.is_empty()
        || !tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
    {
        return Err(
            "ONEDRIVE_TENANT_ID must be a tenant id/domain or a supported authority name".into(),
        );
    }
    Ok(())
}

/// Minimum accepted length for the shared Basic-auth secret.
pub const MIN_BASIC_AUTH_SECRET_LEN: usize = 16;

/// Placeholder values that must never be accepted as a real secret.
const PLACEHOLDER_SECRETS: &[&str] = &["REPLACE_ME", "changeme", "secret", "password"];

/// Fails closed: the daemon refuses to serve unless a real shared secret is
/// configured, or the operator explicitly opts out of authentication with
/// `ONEDRIVE_ALLOW_UNAUTHENTICATED=1` (intended for local development only).
fn validate_basic_auth_secret(
    secret: Option<String>,
    allow_unauthenticated: bool,
) -> Result<Option<String>, String> {
    match secret {
        None if allow_unauthenticated => Ok(None),
        None => Err(
            "ONEDRIVE_BASIC_AUTH_SECRET is not set; refusing to serve unauthenticated. \
             Set a secret (>= 16 chars) or ONEDRIVE_ALLOW_UNAUTHENTICATED=1 for local dev"
                .to_string(),
        ),
        Some(s) if s.len() < MIN_BASIC_AUTH_SECRET_LEN => Err(format!(
            "ONEDRIVE_BASIC_AUTH_SECRET is too short (need at least {MIN_BASIC_AUTH_SECRET_LEN} characters)"
        )),
        Some(s)
            if PLACEHOLDER_SECRETS
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&s)) =>
        {
            Err(
                "ONEDRIVE_BASIC_AUTH_SECRET is a placeholder value; generate a real secret \
             (e.g. `openssl rand -base64 32`)"
                    .to_string(),
            )
        }
        Some(s) => Ok(Some(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_secret_is_rejected_unless_opted_out() {
        assert!(validate_basic_auth_secret(None, false).is_err());
        assert_eq!(validate_basic_auth_secret(None, true).unwrap(), None);
    }

    #[test]
    fn placeholder_and_short_secrets_are_rejected() {
        assert!(validate_basic_auth_secret(Some("REPLACE_ME".into()), false).is_err());
        assert!(validate_basic_auth_secret(Some("replace_me".into()), false).is_err());
        assert!(validate_basic_auth_secret(Some("short".into()), false).is_err());
        assert!(validate_basic_auth_secret(Some("short".into()), true).is_err());
    }

    #[test]
    fn strong_secret_is_accepted() {
        let s = "k9F2mQ7vX1pL4zR8wB3n";
        assert_eq!(
            validate_basic_auth_secret(Some(s.into()), false).unwrap(),
            Some(s.to_string())
        );
    }

    #[test]
    fn drive_base_is_normalized_and_validated() {
        assert_eq!(normalize_drive_base("me/drive").unwrap(), "me/drive");
        assert_eq!(
            normalize_drive_base("/drives/B087983F641B9ED3/").unwrap(),
            "drives/B087983F641B9ED3"
        );
        assert_eq!(
            normalize_drive_base("users/alice@example.com/drive").unwrap(),
            "users/alice@example.com/drive"
        );
        assert_eq!(
            normalize_drive_base("sites/tenant.sharepoint.com,abc,def/drive").unwrap(),
            "sites/tenant.sharepoint.com,abc,def/drive"
        );
        assert!(normalize_drive_base("users/alice/drive/root").is_err());
        assert!(normalize_drive_base("../bad").is_err());
        assert!(normalize_drive_base("").is_err());
        assert!(normalize_drive_base("drives/..").is_err());
        assert!(normalize_drive_base("b!abc%2fother").is_err());
    }

    #[test]
    fn auth_scopes_are_normalized() {
        assert_eq!(
            normalize_auth_scopes(" offline_access   Files.ReadWrite.All  User.Read ").unwrap(),
            "offline_access Files.ReadWrite.All User.Read"
        );
        assert!(normalize_auth_scopes("   ").is_err());
    }
}
