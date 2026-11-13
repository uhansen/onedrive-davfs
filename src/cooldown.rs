use crate::{config::Config, state_file};
use serde::Deserialize;

#[derive(Deserialize)]
struct State {
    version: u32,
    until: u64,
    reason: Option<String>,
    status: Option<u16>,
    consecutive_failures: u32,
}

pub fn load(config: &Config) -> Result<Option<serde_json::Value>, String> {
    state_file::read_file(&config.state_dir, "graph_cooldown.json")?
        .map(|bytes| decode(&bytes, crate::auth::now_secs()))
        .transpose()
}

fn decode(bytes: &[u8], now: u64) -> Result<serde_json::Value, String> {
    let state: State =
        serde_json::from_slice(bytes).map_err(|_| "invalid Graph cooldown state".to_string())?;
    if state.version != 1
        || state
            .status
            .is_some_and(|status| !matches!(status, 429 | 503))
        || state
            .reason
            .as_deref()
            .is_some_and(|reason| !matches!(reason, "retry_after" | "backoff"))
    {
        return Err("unsupported Graph cooldown state".into());
    }
    Ok(serde_json::json!({
        "until": state.until,
        "active": state.until > now,
        "retryAfterSec": state.until.saturating_sub(now),
        "reason": state.reason,
        "status": state.status,
        "consecutiveFailures": state.consecutive_failures,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_is_explicit_and_expiry_is_not_a_sync_success() {
        let bytes = br#"{"version":1,"until":500,"reason":"retry_after","status":429,"consecutive_failures":2}"#;
        let active = decode(bytes, 100).unwrap();
        assert_eq!(active["retryAfterSec"], 400);
        assert_eq!(active["active"], true);
        let expired = decode(bytes, 600).unwrap();
        assert_eq!(expired["active"], false);
        assert_eq!(expired["consecutiveFailures"], 2);
        assert!(decode(b"{}", 0).is_err());
    }
}
