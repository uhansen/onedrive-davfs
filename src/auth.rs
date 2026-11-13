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
    #[serde(default)]
    credential_id: Option<String>,
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

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(rename = "error_description", default)]
    error_description: String,
}

pub(crate) fn now_secs() -> u64 {
    wall_clock::now().seconds
}

fn load(config: &Config) -> Result<TokenFile, String> {
    let bytes = state_file::read_file(&config.state_dir, TOKEN_FILE)?.ok_or_else(|| {
        format!("no {TOKEN_FILE} found in state dir; run tools/device-code-login.sh first")
    })?;
    serde_json::from_slice(&bytes).map_err(|_| format!("invalid {TOKEN_FILE}; sign in again"))
}

fn save(config: &Config, token: &TokenFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(token)
        .map_err(|e| format!("failed to serialize {TOKEN_FILE}: {e}"))?;
    state_file::write_atomic(&config.state_dir, TOKEN_FILE, &bytes)
}

pub fn credential_id(config: &Config) -> Result<Option<String>, String> {
    Ok(load(config)?.credential_id)
}

pub fn credential_status(config: &Config) -> Result<(bool, u64), String> {
    if state_file::read_file(&config.state_dir, TOKEN_FILE)?.is_none() {
        return Ok((false, 0));
    }
    let token = load(config)?;
    Ok((
        !token.refresh_token.is_empty(),
        token.expires_at.saturating_sub(now_secs()),
    ))
}

pub fn bind_legacy_credential(config: &Config, account_id: &str) -> Result<String, String> {
    let mut token = load(config)?;
    if let Some(id) = token.credential_id {
        return Ok(id);
    }
    let id = format!("legacy:{account_id}");
    token.credential_id = Some(id.clone());
    save(config, &token)?;
    Ok(id)
}

fn refresh(config: &Config, token: &TokenFile) -> Result<TokenFile, String> {
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
        urlencode(&token.refresh_token),
        urlencode(&config.auth_scopes),
    );

    let response = http_client::send(HttpRequest {
        method: Method::Post,
        url: &url,
        headers: &[("content-type", "application/x-www-form-urlencoded")],
        body: form.as_bytes(),
    })?;

    if response.status != 200 {
        return Err(format_token_error(response.status, &response.body));
    }

    let parsed: TokenResponse = serde_json::from_slice(&response.body)
        .map_err(|_| "authentication server returned an invalid token response".to_string())?;

    Ok(TokenFile {
        credential_id: token.credential_id.clone(),
        // Graph AD rotates refresh tokens fairly often; persist whichever
        // one we were just handed, falling back to the one we sent if the
        // response omitted it.
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| token.refresh_token.clone()),
        access_token: Some(parsed.access_token),
        expires_at: now_secs() + parsed.expires_in.saturating_sub(EXPIRY_SAFETY_MARGIN_SECS),
    })
}

fn format_token_error(status: u16, body: &[u8]) -> String {
    if let Ok(parsed) = serde_json::from_slice::<TokenErrorResponse>(body) {
        let code = match parsed.error.as_str() {
            code @ ("invalid_grant"
            | "consent_required"
            | "interaction_required"
            | "access_denied"
            | "invalid_client") => code,
            _ => "unknown_error",
        };
        let advice = if parsed.error_description.contains("AADSTS530") {
            "conditional-access or device-compliance policy requires interactive sign-in or administrator action"
        } else {
            match code {
                "invalid_grant" => "sign in again; the credential may be expired or revoked",
                "consent_required" | "interaction_required" => {
                    "browser sign-in or administrator consent is required"
                }
                "access_denied" => "tenant policy denied access; contact your administrator",
                _ => "check app registration, tenant authority, and configured scopes",
            }
        };
        return format!("token refresh failed with status {status}: {code}; {advice}");
    }
    format!("token refresh failed with status {status}; sign in again or check tenant policy")
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

    token = refresh(config, &token)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_error_prefers_oauth_shape() {
        let body =
            br#"{"error":"invalid_grant","error_description":"AADSTS700082: The refresh token has expired."}"#;
        let out = format_token_error(400, body);
        assert!(out.contains("invalid_grant"));
        assert!(out.contains("sign in again"));
        assert!(!out.contains("AADSTS700082"));
        assert!(
            !format_token_error(
                400,
                br#"{"error":"private-data","error_description":"private-token"}"#
            )
            .contains("private")
        );
        assert!(
            format_token_error(
                400,
                br#"{"error":"invalid_grant","error_description":"AADSTS53003 private-token"}"#
            )
            .contains("conditional-access")
        );
    }
}
