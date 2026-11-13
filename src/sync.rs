//! Authenticated `POST /_sync` handler. Driven by a systemd timer because
//! WASIp2 has no background tasks: each invocation fetches a bounded number
//! of Graph `/delta` pages and persists progress.

use crate::config::Config;
use crate::dav::DavResponse;
use crate::graph::{self, DeltaError};
use crate::index::{DriveBinding, Index};
use crate::snapshot::{self, INDEX_FILE, REBUILD_FILE};
use crate::state_file;
use crate::sync_stats::{self, SyncStats, SyncTimings};

pub fn run(config: &Config) -> DavResponse {
    if !config.index_enabled {
        return json_ok(serde_json::json!({
            "skipped": true,
            "reason": "index disabled",
        }));
    }
    match run_inner(config) {
        Ok(report) => json_ok(report),
        Err(error) => {
            let message = match sync_stats::save_failure(config, &error) {
                Ok(()) => error,
                Err(diagnostic_error) => {
                    format!("{error}; could not persist sync diagnostic: {diagnostic_error}")
                }
            };
            DavResponse::error(502, message)
        }
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
    let current_binding = graph::current_drive_binding(config)?;
    let mut timings = SyncTimings {
        binding_ms: now_ms().saturating_sub(started),
        ..SyncTimings::default()
    };
    let load_started = now_ms();
    let live_meta = snapshot::read_meta(&config.state_dir)?;
    let can_probe = live_meta.as_ref().is_some_and(|meta| {
        meta.crawl_complete
            && meta.pending_next_link.is_none()
            && meta.delta_token.is_some()
            && meta.root_id.is_some()
            && meta.built_at != 0
            && meta.total_dirs != 0
            && meta.binding.as_ref() == Some(&current_binding)
    }) && !state_file::exists(&config.state_dir, REBUILD_FILE)?;
    let probe_started = now_ms();
    let mut first_page = if can_probe {
        Some(graph::delta(
            config,
            live_meta.as_ref().unwrap().delta_token.as_ref().unwrap(),
        ))
    } else {
        None
    };
    timings.delta_ms = now_ms().saturating_sub(probe_started);
    let unchanged = match (live_meta.as_ref(), first_page.as_ref()) {
        (Some(meta), Some(Ok(page))) => unchanged_delta(meta, page),
        _ => false,
    };
    // Identity is still revalidated every tick. Only a genuinely unchanged
    // complete cursor can skip decoding the full namespace.
    let (live, rebuild) = if unchanged {
        let meta = live_meta.as_ref().unwrap();
        (
            Some(Index {
                binding: meta.binding.clone(),
                root: meta.root_id.clone(),
                delta_token: meta.delta_token.clone(),
                generation: meta.generation,
                namespace_revision: meta.namespace_revision,
                crawl_complete: true,
                ..Index::default()
            }),
            None,
        )
    } else {
        (
            snapshot::load(&config.state_dir, INDEX_FILE)?,
            snapshot::load(&config.state_dir, REBUILD_FILE)?,
        )
    };
    timings.snapshot_load_ms = now_ms()
        .saturating_sub(load_started)
        .saturating_sub(timings.delta_ms);
    timings.snapshot_load_skipped = unchanged;
    let has_live = live
        .as_ref()
        .is_some_and(|idx| idx.crawl_complete || idx.root.is_some());

    let binding_changed = existing_binding_mismatch(live_meta.as_ref(), &current_binding)
        || rebuild.as_ref().is_some_and(|idx| {
            !idx.binding
                .as_ref()
                .is_some_and(|binding| binding.same_target(&current_binding))
        });
    let live = if binding_changed {
        let generation = live
            .as_ref()
            .map(|idx| idx.generation)
            .into_iter()
            .chain(rebuild.as_ref().map(|idx| idx.generation))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let idx = Index {
            binding: Some(current_binding.clone()),
            generation,
            namespace_revision: live
                .as_ref()
                .map(|idx| idx.namespace_revision)
                .into_iter()
                .chain(rebuild.as_ref().map(|idx| idx.namespace_revision))
                .max()
                .unwrap_or(0),
            ..Index::default()
        };
        Some(idx)
    } else {
        live
    };
    let rebuild = if binding_changed { None } else { rebuild };

    let mut target = if rebuild.is_some() {
        REBUILD_FILE
    } else {
        INDEX_FILE
    };
    let mut idx = rebuild.or(live).unwrap_or_default();
    let metadata_changed = idx.binding.as_ref() != Some(&current_binding);
    if metadata_changed {
        idx.binding = Some(current_binding.clone());
    }
    let mut url = idx
        .pending_next_link
        .clone()
        .or_else(|| idx.delta_token.clone())
        .unwrap_or_else(|| graph::initial_delta_url(&config.drive_base));

    let mut pages = 0usize;
    let mut applied = 0usize;
    let mut dirty = binding_changed || metadata_changed;
    let mut namespace_changed = binding_changed || metadata_changed;
    if let Some(meta) = live_meta.as_ref() {
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
        let delta_started = now_ms();
        let page = match first_page.take() {
            Some(page) => page,
            None => {
                let page = graph::delta(config, &url);
                timings.delta_ms += now_ms().saturating_sub(delta_started);
                page
            }
        };
        let apply_started = now_ms();
        match page {
            Ok(page) => {
                pages += 1;
                if !page.items.is_empty() {
                    dirty = true;
                    namespace_changed = true;
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
                        if !unchanged {
                            idx.sweep_orphans();
                        }
                        complete = true;
                        timings.apply_ms += now_ms().saturating_sub(apply_started);
                        break;
                    }
                }
            }
            Err(DeltaError::Resync) => {
                resync = true;
                namespace_changed = true;
                dirty = true;
                pages += 1;
                let generation = idx.generation.saturating_add(1);
                idx = Index {
                    binding: Some(current_binding.clone()),
                    generation,
                    namespace_revision: idx.namespace_revision,
                    ..Index::default()
                };
                url = graph::initial_delta_url(&config.drive_base);
                complete = false;
                // Keep serving the existing live snapshot until the shadow
                // crawl finishes, unless there is no live snapshot yet.
                target = if has_live { REBUILD_FILE } else { INDEX_FILE };
            }
            Err(DeltaError::Other(e)) => return Err(e),
        }
        timings.apply_ms += now_ms().saturating_sub(apply_started);
    }

    let save_started = now_ms();
    if dirty {
        if namespace_changed {
            idx.advance_namespace()?;
        }
        snapshot::save(&config.state_dir, target, &idx)?;
    }
    if binding_changed
        && target != REBUILD_FILE
        && state_file::read_file(&config.state_dir, REBUILD_FILE)?.is_some()
    {
        state_file::remove_file(&config.state_dir, REBUILD_FILE)?;
    }
    if complete && target == REBUILD_FILE {
        state_file::rename(&config.state_dir, REBUILD_FILE, INDEX_FILE)?;
        target = INDEX_FILE;
    }
    timings.snapshot_save_ms = now_ms().saturating_sub(save_started);
    let timings = config.perf_log.then_some(timings);

    let duration_ms = now_ms().saturating_sub(started);
    let items_per_sec = if duration_ms > 0 {
        (applied as f64) * 1000.0 / (duration_ms as f64)
    } else {
        0.0
    };
    let stats = SyncStats {
        binding: Some(current_binding.clone()),
        applied: applied as u64,
        pages: pages as u64,
        complete,
        duration_ms,
        resync,
        items_per_sec,
        at: now_secs(),
        has_token: idx.delta_token.is_some(),
        target: target.to_string(),
        timings: timings.clone(),
    };
    sync_stats::save(&config.state_dir, &stats)?;
    sync_stats::clear_failure(config)?;

    Ok(serde_json::json!({
        "applied": applied,
        "pages": pages,
        "complete": complete,
        "has_token": idx.delta_token.is_some(),
        "bindingReset": binding_changed,
        "drive": {
            "tenantId": current_binding.tenant_id,
            "selector": current_binding.drive_base,
            "id": current_binding.drive_id,
            "name": current_binding.drive_name,
            "type": current_binding.drive_type,
            "webUrl": current_binding.web_url,
            "owner": current_binding.owner_label,
        },
        "duration_ms": duration_ms,
        "resync": resync,
        "target": target,
        "items_per_sec": items_per_sec,
        "timings": timings,
    }))
}

fn unchanged_delta(meta: &snapshot::SnapshotMeta, page: &graph::DeltaPage) -> bool {
    page.items.is_empty()
        && page.next_link.is_none()
        && page.delta_token.is_some()
        && page.delta_token == meta.delta_token
}

fn existing_binding_mismatch(
    meta: Option<&snapshot::SnapshotMeta>,
    current: &DriveBinding,
) -> bool {
    match meta {
        None => false,
        Some(meta) => match meta.binding.as_ref() {
            Some(saved) => !saved.same_target(current),
            None => true,
        },
    }
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
    fn only_identical_empty_final_delta_can_skip_loading_snapshot() {
        let meta = snapshot::SnapshotMeta {
            binding: None,
            delta_token: Some("cursor".into()),
            pending_next_link: None,
            crawl_complete: true,
            generation: 1,
            namespace_revision: 0,
            built_at: 1,
            root_id: Some("root".into()),
            total_dirs: 1,
            total_files: 0,
        };
        let mut page = graph::DeltaPage {
            items: vec![],
            next_link: None,
            delta_token: Some("cursor".into()),
        };
        assert!(unchanged_delta(&meta, &page));
        page.delta_token = Some("new-cursor".into());
        assert!(!unchanged_delta(&meta, &page));
        page.delta_token = None;
        assert!(!unchanged_delta(&meta, &page));
        page.delta_token = Some("cursor".into());
        page.next_link = Some("next-page".into());
        assert!(!unchanged_delta(&meta, &page));
        page.next_link = None;
        page.items.push(crate::index::DeltaItem {
            id: "removed".into(),
            parent_id: None,
            name: String::new(),
            deleted: true,
            is_root: false,
            meta: crate::index::ItemMeta {
                is_dir: false,
                size: 0,
                mtime: 0,
                etag: String::new(),
            },
        });
        assert!(!unchanged_delta(&meta, &page));
    }

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

    #[test]
    fn legacy_or_changed_binding_triggers_rebuild() {
        let current = DriveBinding {
            account_id: "user-a".into(),
            credential_id: "login-a".into(),
            client_id: None,
            tenant_id: "common".into(),
            drive_base: "me/drive".into(),
            drive_id: "drive-a".into(),
            drive_name: "Docs".into(),
            drive_type: "business".into(),
            web_url: String::new(),
            owner_label: String::new(),
        };
        let legacy = snapshot::SnapshotMeta {
            binding: None,
            delta_token: None,
            pending_next_link: None,
            crawl_complete: true,
            generation: 1,
            namespace_revision: 0,
            built_at: 1,
            root_id: None,
            total_dirs: 0,
            total_files: 0,
        };
        assert!(existing_binding_mismatch(Some(&legacy), &current));

        let same = snapshot::SnapshotMeta {
            binding: Some(current.clone()),
            ..legacy.clone()
        };
        assert!(!existing_binding_mismatch(Some(&same), &current));
    }
}
