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
    pub basic_auth_secret: Option<String>,
    /// Preopened directory used to persist `token.json`. Must be granted to
    /// the component via `wasmtime serve --dir <path>::/state`.
    pub state_dir: Descriptor,
}

fn env(name: &str) -> Option<String> {
    environment::get_environment()
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

impl Config {
    /// Loads configuration from the environment and preopens. A client id
    /// is optional at startup so a pre-seeded, still-valid access token can
    /// be used immediately; only token refresh requires `ONEDRIVE_CLIENT_ID`.
    pub fn load() -> Result<Self, String> {
        let client_id = env("ONEDRIVE_CLIENT_ID");
        let tenant_id = env("ONEDRIVE_TENANT_ID").unwrap_or_else(|| "common".to_string());
        let drive_base = env("ONEDRIVE_DRIVE_BASE").unwrap_or_else(|| "me/drive".to_string());
        let basic_auth_secret = validate_basic_auth_secret(
            env("ONEDRIVE_BASIC_AUTH_SECRET"),
            env("ONEDRIVE_ALLOW_UNAUTHENTICATED").as_deref() == Some("1"),
        )?;

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
            basic_auth_secret,
            state_dir,
        })
    }
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
}
