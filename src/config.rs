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
        let basic_auth_secret = env("ONEDRIVE_BASIC_AUTH_SECRET");

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
