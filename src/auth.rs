//! OAuth token handling.
//!
//! This module deliberately does **not** implement the interactive/device
//! code consent flow -- that happens once, outside the sandbox, via
//! `tools/device-code-login.sh`. All this module does is:
//!   1. load the refresh token (and any still-valid access token) from
//!      `{state_dir}/token.json`,
//!   2. refresh the access token via Microsoft's OAuth2 token endpoint when
//!      it is missing or expired, persisting the (possibly rotated) refresh
//!      token back to the same file,
//!   3. hand back a bearer token string for `graph.rs` to use.

use serde::{Deserialize, Serialize};

use crate::bindings::wasi::clocks::wall_clock;
use crate::bindings::wasi::http::types::Method;
use crate::config::Config;
use crate::http_client::{self, HttpRequest};
use crate::state_file;

const TOKEN_FILE: &str = "token.json";
/// Refresh a little early to avoid racing expiry mid-request.
const EXPIRY_SAFETY_MARGIN_SECS: u64 = 60;

#[derive(Debug, Serialize, Deserialize)]
struct TokenFile {
    refresh_token: String,
    #[serde(default)]
    access_token: Option<String>,
    /// Unix timestamp (seconds) after which `access_token` must be treated
    /// as expired.
    #[serde(default)]
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

fn now_secs() -> u64 {
    wall_clock::now().seconds
}

fn load(config: &Config) -> Result<TokenFile, String> {
    let bytes = state_file::read_file(&config.state_dir, TOKEN_FILE)?.ok_or_else(|| {
        format!("no {TOKEN_FILE} found in state dir; run tools/device-code-login.sh first")
    })?;
    serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse {TOKEN_FILE}: {e}"))
}

fn save(config: &Config, token: &TokenFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(token)
        .map_err(|e| format!("failed to serialize {TOKEN_FILE}: {e}"))?;
    state_file::write_file(&config.state_dir, TOKEN_FILE, &bytes)
}

fn refresh(config: &Config, refresh_token: &str) -> Result<TokenFile, String> {
    let client_id = config
        .client_id
        .as_deref()
        .ok_or("missing required env var ONEDRIVE_CLIENT_ID for token refresh")?;
    let url = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        config.tenant_id
    );
    let form = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}&scope={}",
        urlencode(client_id),
        urlencode(refresh_token),
        urlencode("offline_access Files.ReadWrite.All User.Read"),
    );

    let response = http_client::send(HttpRequest {
        method: Method::Post,
        url: &url,
        headers: &[("content-type", "application/x-www-form-urlencoded")],
        body: form.as_bytes(),
    })?;

    if response.status != 200 {
        return Err(format!(
            "token refresh failed with status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ));
    }

    let parsed: TokenResponse = serde_json::from_slice(&response.body)
        .map_err(|e| format!("failed to parse token response: {e}"))?;

    Ok(TokenFile {
        // Graph AD rotates refresh tokens fairly often; persist whichever
        // one we were just handed, falling back to the one we sent if the
        // response omitted it.
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        access_token: Some(parsed.access_token),
        expires_at: now_secs() + parsed.expires_in.saturating_sub(EXPIRY_SAFETY_MARGIN_SECS),
    })
}

/// Returns a valid `Authorization: Bearer ...` value, refreshing the
/// access token first if needed.
pub fn bearer_token(config: &Config) -> Result<String, String> {
    let mut token = load(config)?;

    if let Some(access_token) = token.access_token.as_ref() {
        if now_secs() < token.expires_at {
            return Ok(format!("Bearer {access_token}"));
        }
    }

    token = refresh(config, &token.refresh_token)?;
    save(config, &token)?;

    Ok(format!(
        "Bearer {}",
        token
            .access_token
            .as_ref()
            .expect("access_token set by refresh() above")
    ))
}

/// Minimal `application/x-www-form-urlencoded` percent-encoder -- good
/// enough for the small, known-shape values (ids, scopes, tokens) used
/// above.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
