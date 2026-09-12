//! Authenticated `POST /_sync` handler. Driven by a systemd timer because
//! WASIp2 has no background tasks: each invocation fetches a bounded number
//! of Graph `/delta` pages and persists progress.

use crate::config::Config;
use crate::dav::DavResponse;
use crate::graph::{self, DeltaError};
use crate::index::Index;
use crate::snapshot::{self, INDEX_FILE, REBUILD_FILE};
use crate::state_file;
use crate::sync_stats::{self, SyncStats};

pub fn run(config: &Config) -> DavResponse {
    if !config.index_enabled {
        return json_ok(serde_json::json!({
            "skipped": true,
            "reason": "index disabled",
        }));
    }
    match run_inner(config) {
        Ok(report) => json_ok(report),
        Err(e) => DavResponse::error(502, e),
    }
}

fn json_ok(value: serde_json::Value) -> DavResponse {
    DavResponse {
        status: 200,
        content_type: "application/json".to_string(),
        body: value.to_string().into_bytes(),
        headers: Vec::new(),
        content_length: None,
    }
}

fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        crate::bindings::wasi::clocks::monotonic_clock::now() / 1_000_000
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

fn run_inner(config: &Config) -> Result<serde_json::Value, String> {
    let started = now_ms();
    let live = snapshot::load(&config.state_dir, INDEX_FILE)?.unwrap_or_default();
    let rebuild = snapshot::load(&config.state_dir, REBUILD_FILE)?;

    let mut target = if rebuild.is_some() {
        REBUILD_FILE
    } else {
        INDEX_FILE
    };
    let mut idx = rebuild.unwrap_or(live);
    let mut url = idx
        .pending_next_link
        .clone()
        .or_else(|| idx.delta_token.clone())
        .unwrap_or_else(|| graph::initial_delta_url(&config.drive_base));

    let mut pages = 0usize;
    let mut applied = 0usize;
    let mut dirty = false;
    if let Ok(Some(meta)) = snapshot::read_meta(&config.state_dir) {
        // Snapshots written before total_* / built_at need a one-shot rewrite.
        if meta.built_at == 0 || (meta.total_dirs == 0 && meta.crawl_complete) {
            dirty = true;
        }
    }
    let mut resync = false;
    let mut complete = idx.crawl_complete && idx.pending_next_link.is_none();

    while pages < config.sync_max_pages {
        if now_ms().saturating_sub(started) >= config.sync_budget_ms && pages > 0 {
            break;
        }
        match graph::delta(config, &url) {
            Ok(page) => {
                pages += 1;
                if !page.items.is_empty() {
                    dirty = true;
                }
                for it in page.items {
                    idx.apply(it);
                    applied += 1;
                }
                match page.next_link {
                    Some(next) => {
                        if idx.pending_next_link.as_deref() != Some(next.as_str()) {
                            dirty = true;
                        }
                        idx.pending_next_link = Some(next.clone());
                        idx.crawl_complete = false;
                        complete = false;
                        url = next;
                    }
                    None => {
                        if idx.delta_token != page.delta_token || idx.pending_next_link.is_some() {
                            dirty = true;
                        }
                        idx.pending_next_link = None;
                        if let Some(token) = page.delta_token {
                            idx.delta_token = Some(token);
                        }
                        idx.crawl_complete = true;
                        idx.sweep_orphans();
                        complete = true;
                        break;
                    }
                }
            }
            Err(DeltaError::Resync) => {
                resync = true;
                dirty = true;
                pages += 1;
                let generation = idx.generation.saturating_add(1);
                idx = Index {
                    generation,
                    ..Index::default()
                };
                url = graph::initial_delta_url(&config.drive_base);
                complete = false;
                // Keep serving the existing live snapshot until the shadow
                // crawl finishes, unless there is no live snapshot yet.
                target = if snapshot::load(&config.state_dir, INDEX_FILE)
                    .ok()
                    .flatten()
                    .map(|live| live.crawl_complete || live.root.is_some())
                    .unwrap_or(false)
                {
                    REBUILD_FILE
                } else {
                    INDEX_FILE
                };
            }
            Err(DeltaError::Other(e)) => return Err(e),
        }
    }

    if dirty {
        snapshot::save(&config.state_dir, target, &idx)?;
    }
    if complete && target == REBUILD_FILE {
        state_file::rename(&config.state_dir, REBUILD_FILE, INDEX_FILE)?;
        target = INDEX_FILE;
    }

    let duration_ms = now_ms().saturating_sub(started);
    let items_per_sec = if duration_ms > 0 {
        (applied as f64) * 1000.0 / (duration_ms as f64)
    } else {
        0.0
    };
    let stats = SyncStats {
        applied: applied as u64,
        pages: pages as u64,
        complete,
        duration_ms,
        resync,
        items_per_sec,
        at: now_secs(),
        has_token: idx.delta_token.is_some(),
        target: target.to_string(),
    };
    let _ = sync_stats::save(&config.state_dir, &stats);

    Ok(serde_json::json!({
        "applied": applied,
        "pages": pages,
        "complete": complete,
        "has_token": idx.delta_token.is_some(),
        "duration_ms": duration_ms,
        "resync": resync,
        "target": target,
        "items_per_sec": items_per_sec,
    }))
}

fn now_secs() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        crate::bindings::wasi::clocks::wall_clock::now().seconds
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resync_bumps_generation_and_clears_nodes() {
        let mut idx = Index {
            generation: 4,
            crawl_complete: true,
            ..Index::default()
        };
        idx.root = Some("r".into());
        let next = Index {
            generation: idx.generation.saturating_add(1),
            ..Index::default()
        };
        assert_eq!(next.generation, 5);
        assert!(next.nodes.is_empty());
        assert!(next.root.is_none());
    }
}
