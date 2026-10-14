//! Last `/_sync` tick, persisted because each WASIp2 request is a fresh
//! instance with no surviving in-memory stats.

use crate::bindings::wasi::filesystem::types::Descriptor;
use crate::config::Config;
use crate::index::DriveBinding;
use crate::state_file;
use serde::{Deserialize, Serialize};

pub const STATS_FILE: &str = "sync_stats.json";
const FAILURE_FILE: &str = "sync_failure.json";

#[derive(Serialize, Deserialize)]
pub struct SyncFailure {
    pub error: String,
    pub at: u64,
    tenant_id: String,
    client_id: Option<String>,
    drive_base: String,
    credential_id: Option<String>,
}

pub fn save_failure(config: &Config, error: &str) -> Result<(), String> {
    let failure = SyncFailure {
        error: error.to_string(),
        at: crate::auth::now_secs(),
        tenant_id: config.tenant_id.clone(),
        client_id: config.client_id.clone(),
        drive_base: config.drive_base.clone(),
        credential_id: crate::auth::credential_id(config)?,
    };
    let bytes = serde_json::to_vec(&failure).map_err(|error| error.to_string())?;
    state_file::write_atomic(&config.state_dir, FAILURE_FILE, &bytes)
}

pub fn load_failure(config: &Config) -> Result<Option<SyncFailure>, String> {
    let Some(bytes) = state_file::read_file(&config.state_dir, FAILURE_FILE)? else {
        return Ok(None);
    };
    let failure: SyncFailure = serde_json::from_slice(&bytes)
        .map_err(|error| format!("sync failure diagnostic: {error}"))?;
    if failure.tenant_id != config.tenant_id
        || failure.client_id != config.client_id
        || failure.drive_base != config.drive_base
        || failure.credential_id != crate::auth::credential_id(config)?
    {
        return Ok(None);
    }
    Ok(Some(failure))
}

pub fn clear_failure(config: &Config) -> Result<(), String> {
    if state_file::read_file(&config.state_dir, FAILURE_FILE)?.is_some() {
        state_file::remove_file(&config.state_dir, FAILURE_FILE)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SyncStats {
    #[serde(default)]
    pub binding: Option<DriveBinding>,
    pub applied: u64,
    pub pages: u64,
    pub complete: bool,
    pub duration_ms: u64,
    pub resync: bool,
    pub items_per_sec: f64,
    pub at: u64,
    pub has_token: bool,
    pub target: String,
}

impl SyncStats {
    pub fn matches_binding(&self, binding: &DriveBinding) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|saved| saved.same_target(binding))
    }
}

pub fn save(dir: &Descriptor, stats: &SyncStats) -> Result<(), String> {
    let bytes = serde_json::to_vec(stats).map_err(|e| e.to_string())?;
    state_file::write_atomic(dir, STATS_FILE, &bytes)
}

pub fn load(dir: &Descriptor) -> Result<Option<SyncStats>, String> {
    match state_file::read_file(dir, STATS_FILE)? {
        Some(bytes) => {
            let stats =
                serde_json::from_slice(&bytes).map_err(|e| format!("sync_stats.json: {e}"))?;
            Ok(Some(stats))
        }

        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_statistics_are_not_reused_across_login_or_drive_changes() {
        let binding = DriveBinding {
            account_id: "fixture-account".into(),
            credential_id: "fixture-login".into(),
            drive_id: "b!fixture-one".into(),
            ..DriveBinding::default()
        };
        let legacy = SyncStats::default();
        assert!(!legacy.matches_binding(&binding));
        let tick = SyncStats {
            binding: Some(binding.clone()),
            ..SyncStats::default()
        };
        assert!(tick.matches_binding(&binding));
        let mut changed = binding.clone();
        changed.credential_id = "new-login".into();
        assert!(!tick.matches_binding(&changed));
        changed = binding;
        changed.drive_id = "b!fixture-two".into();
        assert!(!tick.matches_binding(&changed));
    }
}
