//! Last `/_sync` tick, persisted because each WASIp2 request is a fresh
//! instance with no surviving in-memory stats.

use crate::bindings::wasi::filesystem::types::Descriptor;
use crate::state_file;
use serde::{Deserialize, Serialize};

pub const STATS_FILE: &str = "sync_stats.json";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SyncStats {
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

pub fn save(dir: &Descriptor, stats: &SyncStats) -> Result<(), String> {
    let bytes = serde_json::to_vec(stats).map_err(|e| e.to_string())?;
    state_file::write_atomic(dir, STATS_FILE, &bytes)
}

pub fn load(dir: &Descriptor) -> Result<Option<SyncStats>, String> {
    match state_file::read_file(dir, STATS_FILE)? {
        Some(bytes) => {
            let stats = serde_json::from_slice(&bytes)
                .map_err(|e| format!("sync_stats.json: {e}"))?;
            Ok(Some(stats))
        }
        None => Ok(None),
    }
}
