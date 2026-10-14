//! Authenticated JSON endpoints for the Omarchy plugin.
//!
//! Gated on `GET` + `X-OneDrive-Plugin: 1` so a real OneDrive folder named
//! `_status` / `_tree` is never shadowed for davfs2.

use crate::config::Config;
use crate::dav::DavResponse;
use crate::graph;
use crate::index;
use crate::snapshot::{self, ChildInfo, Lookup};
use crate::sync_stats;
use crate::xml;

pub fn status(config: &Config) -> DavResponse {
    let (credential_present, token_expires_in) = match crate::auth::credential_status(config) {
        Ok(status) => status,
        Err(error) => return json_ok(serde_json::json!({"ok": false, "error": error})),
    };
    let failure = match sync_stats::load_failure(config) {
        Ok(failure) => failure,
        Err(error) => return json_ok(serde_json::json!({"ok": false, "error": error})),
    };
    if !config.index_enabled {
        return json_ok(serde_json::json!({
            "ok": true,
            "indexEnabled": false,
            "credentialPresent": credential_present,
            "tokenExpiresInSec": token_expires_in,
            "lastSyncError": failure.as_ref().map(|failure| &failure.error),
            "lastSyncErrorAt": failure.as_ref().map(|failure| failure.at),
        }));
    }
    let meta = match snapshot::read_meta(&config.state_dir) {
        Ok(meta) => meta,
        Err(error) => return json_ok(serde_json::json!({"ok": false, "error": error})),
    };
    if !config.cache_current() {
        return json_ok(serde_json::json!({
            "ok": true,
            "indexEnabled": true,
            "present": false,
            "accessVerified": false,
            "reindexRequired": true,
            "crawlComplete": false,
            "credentialPresent": credential_present,
            "tokenExpiresInSec": token_expires_in,
            "lastSyncError": failure.as_ref().map(|failure| &failure.error),
            "lastSyncErrorAt": failure.as_ref().map(|failure| failure.at),
        }));
    }

    let last_tick = match sync_stats::load(&config.state_dir) {
        Ok(tick) => tick.filter(|tick| {
            meta.as_ref()
                .and_then(|meta| meta.binding.as_ref())
                .is_some_and(|binding| tick.matches_binding(binding))
        }),
        Err(error) => return json_ok(serde_json::json!({"ok": false, "error": error})),
    };
    let rebuilding = last_tick
        .as_ref()
        .is_some_and(|tick| tick.target == snapshot::REBUILD_FILE && !tick.complete);

    let mut body = serde_json::json!({
        "ok": true,
        "indexEnabled": true,
        "present": meta.is_some(),
        "accessVerified": true,
        "reindexRequired": false,
        "rebuildInProgress": rebuilding,
        "credentialPresent": credential_present,
        "tokenExpiresInSec": token_expires_in,
        "lastSyncError": failure.as_ref().map(|failure| &failure.error),
        "lastSyncErrorAt": failure.as_ref().map(|failure| failure.at),
    });
    if let Some(meta) = meta {
        body["crawlComplete"] = serde_json::json!(meta.crawl_complete && !rebuilding);
        body["hasToken"] = serde_json::json!(meta.delta_token.is_some());
        body["generation"] = serde_json::json!(meta.generation);
        body["builtAt"] = serde_json::json!(meta.built_at);
        body["totalDirs"] = serde_json::json!(meta.total_dirs);
        body["totalFiles"] = serde_json::json!(meta.total_files);
        body["pendingNextLink"] = serde_json::json!(meta.pending_next_link.is_some());
        if let Some(binding) = meta.binding {
            body["drive"] = serde_json::json!({
                "tenantId": binding.tenant_id,
                "selector": binding.drive_base,
                "id": binding.drive_id,
                "name": binding.drive_name,
                "type": binding.drive_type,
                "webUrl": binding.web_url,
                "owner": binding.owner_label,
            });
        }
    }
    if let Some(tick) = last_tick {
        body["lastTick"] = serde_json::json!({
            "applied": tick.applied,
            "pages": tick.pages,
            "complete": tick.complete,
            "durationMs": tick.duration_ms,
            "resync": tick.resync,
            "itemsPerSec": tick.items_per_sec,
            "at": tick.at,
        });
    }
    json_ok(body)
}

pub fn libraries(config: &Config, query: Option<&str>) -> DavResponse {
    let group_id = query_param(query, "groupId");
    let site_id = query_param(query, "siteId");
    let site_url = query_param(query, "siteUrl");

    let scope_count = group_id.is_some() as u8 + site_id.is_some() as u8 + site_url.is_some() as u8;
    if scope_count > 1 {
        return json_ok(serde_json::json!({
            "ok": false,
            "error": "specify at most one of groupId, siteId, or siteUrl",
        }));
    }

    let (scope_name, resolved_site, libraries) = if let Some(group_id) = group_id.as_deref() {
        (
            "group",
            None,
            graph::list_libraries(config, graph::LibraryScope::Group(group_id)),
        )
    } else if let Some(site_id) = site_id.as_deref() {
        (
            "site",
            None,
            graph::list_libraries(config, graph::LibraryScope::Site(site_id)),
        )
    } else if let Some(site_url) = site_url.as_deref() {
        match graph::resolve_site_by_url(config, site_url) {
            Ok(site) => {
                let site_id = site.id.clone();
                (
                    "siteUrl",
                    Some(site),
                    graph::list_libraries(config, graph::LibraryScope::Site(&site_id)),
                )
            }
            Err(error) => {
                return json_ok(serde_json::json!({
                    "ok": false,
                    "error": error,
                }));
            }
        }
    } else {
        (
            "me",
            None,
            graph::list_libraries(config, graph::LibraryScope::Me),
        )
    };

    match libraries {
        Ok(libraries) => json_ok(serde_json::json!({
            "ok": true,
            "scope": scope_name,
            "resolvedSite": resolved_site.as_ref().map(|site| serde_json::json!({
                "id": site.id,
                "displayName": site.display_name,
                "webUrl": site.web_url,
                "hostname": site.hostname,
                "path": site.path,
            })),
            "libraries": libraries.into_iter().map(|library| serde_json::json!({
                "selector": library.selector,
                "id": library.drive_id,
                "name": library.drive_name,
                "type": library.drive_type,
                "webUrl": library.web_url,
                "owner": library.owner_label,
            })).collect::<Vec<_>>(),
        })),
        Err(error) => json_ok(serde_json::json!({
            "ok": false,
            "error": error,
        })),
    }
}

pub fn resolve(config: &Config, query: Option<&str>) -> DavResponse {
    let raw = query_param(query, "selector").unwrap_or_default();
    let selector = match crate::config::normalize_drive_base(&raw) {
        Ok(selector) => selector,
        Err(error) => return DavResponse::error(400, error),
    };
    match graph::resolve_drive(config, &selector) {
        Ok(binding) => json_ok(serde_json::json!({
            "ok": true,
            "drive": {
                "accountId": binding.account_id,
                "credentialId": binding.credential_id,
                "tenantId": binding.tenant_id,
                "selector": format!("drives/{}", binding.drive_id),
                "id": binding.drive_id,
                "name": binding.drive_name,
                "type": binding.drive_type,
                "webUrl": binding.web_url,
                "owner": binding.owner_label,
            }
        })),
        Err(error) => DavResponse::error(502, error),
    }
}

const SEARCH_MAX_RESULTS: usize = 100;
const SEARCH_MIN_QUERY_LEN: usize = 2;

pub fn search(config: &Config, query: Option<&str>) -> DavResponse {
    let q = query_param(query, "q").unwrap_or_default();

    if !config.cache_current() {
        return json_ok(serde_json::json!({"ok": true, "ready": false}));
    }

    let index = match snapshot::load(&config.state_dir, snapshot::INDEX_FILE) {
        Ok(Some(idx)) => idx,
        Ok(None) => return json_ok(serde_json::json!({"ok": true, "ready": false})),
        Err(_) => return json_ok(serde_json::json!({"ok": true, "ready": false})),
    };

    json_ok(search_index(&index, &q))
}

/// Pure name-substring search over an already-loaded index. Split out from
/// `search` so it's testable without a WASI `Config`/`Descriptor`.
fn search_index(index: &index::Index, q: &str) -> serde_json::Value {
    if q.chars().count() < SEARCH_MIN_QUERY_LEN {
        return serde_json::json!({
            "ok": true,
            "ready": true,
            "query": q,
            "truncated": false,
            "results": [],
        });
    }

    let needle = q.to_lowercase();
    let mut matches: Vec<(String, &index::Node)> = index
        .nodes
        .iter()
        .filter(|(_, node)| node.parent.is_some() && node.name.to_lowercase().contains(&needle))
        .filter_map(|(id, node)| index.path_of(id).map(|path| (path, node)))
        .collect();

    matches
        .sort_by(|(pa, na), (pb, nb)| nb.meta.is_dir.cmp(&na.meta.is_dir).then_with(|| pa.cmp(pb)));

    let truncated = matches.len() > SEARCH_MAX_RESULTS;
    matches.truncate(SEARCH_MAX_RESULTS);

    let results: Vec<serde_json::Value> = matches
        .into_iter()
        .map(|(path, node)| {
            serde_json::json!({
                "path": path,
                "name": node.name,
                "isDir": node.meta.is_dir,
                "size": node.meta.size,
                "mtime": node.meta.mtime,
            })
        })
        .collect();

    serde_json::json!({
        "ok": true,
        "ready": true,
        "query": q,
        "truncated": truncated,
        "results": results,
    })
}

pub fn tree(config: &Config, query: Option<&str>, cache_only: bool) -> DavResponse {
    let raw = query_param(query, "path").unwrap_or_else(|| "/".to_string());
    let path = match crate::sanitize_decoded_path(&raw) {
        Ok(p) => p,
        Err(e) => return DavResponse::error(400, e),
    };

    if config.cache_current() {
        match snapshot::lookup_file(&config.state_dir, &path) {
            Ok(Some(hit)) => return json_ok(lookup_to_tree(&path, hit, "index")),
            Ok(None) => {}
            Err(error) => return DavResponse::error(502, error),
        }
    }

    if cache_only {
        return json_ok(serde_json::json!({
            "ok": false,
            "error": "Library index is updating; retry browsing when this folder is indexed",
        }));
    }

    match graph_tree(config, &path) {
        Ok(value) => json_ok(value),
        Err(e) if e == "not found" => DavResponse::error(404, "not found"),
        Err(e) => DavResponse::error(502, e),
    }
}

fn graph_tree(config: &Config, path: &str) -> Result<serde_json::Value, String> {
    let item = graph::stat(config, path)?;
    let name = if path.trim_matches('/').is_empty() {
        String::new()
    } else {
        item.name.clone()
    };
    let mut children = Vec::new();
    if item.is_dir {
        for child in graph::children(config, path)? {
            if !child.is_dir {
                continue;
            }
            children.push(child_json(
                path,
                &child.name,
                true,
                child.size,
                child.last_modified,
                &child.etag,
            ));
        }
    }
    Ok(serde_json::json!({
        "ok": true,
        "path": index::normalize_path(path),
        "name": name,
        "isDir": item.is_dir,
        "size": item.size,
        "mtime": item.last_modified,
        "etag": item.etag,
        "source": "graph",
        "children": children,
    }))
}

fn lookup_to_tree(path: &str, hit: Lookup, source: &str) -> serde_json::Value {
    match hit {
        Lookup::Dir { info, children } => {
            let kids: Vec<serde_json::Value> = children
                .iter()
                .filter(|c| c.is_dir)
                .map(|c| child_json(path, &c.name, true, c.size, c.mtime, &c.etag))
                .collect();
            dir_json(path, &info, kids, source)
        }
        Lookup::File(info) => serde_json::json!({
            "ok": true,
            "path": index::normalize_path(path),
            "name": info.name,
            "isDir": false,
            "size": info.size,
            "mtime": info.mtime,
            "etag": info.etag,
            "source": source,
            "children": [],
        }),
    }
}

fn dir_json(
    path: &str,
    info: &ChildInfo,
    children: Vec<serde_json::Value>,
    source: &str,
) -> serde_json::Value {
    let name = if path.trim_matches('/').is_empty() {
        String::new()
    } else {
        info.name.clone()
    };
    serde_json::json!({
        "ok": true,
        "path": index::normalize_path(path),
        "name": name,
        "isDir": true,
        "size": info.size,
        "mtime": info.mtime,
        "etag": info.etag,
        "source": source,
        "children": children,
    })
}

fn child_json(
    parent: &str,
    name: &str,
    is_dir: bool,
    size: u64,
    mtime: u64,
    etag: &str,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "path": join_child_path(parent, name),
        "isDir": is_dir,
        "size": size,
        "mtime": mtime,
        "etag": etag,
    })
}

pub(crate) fn join_child_path(parent: &str, name: &str) -> String {
    if parent.trim_matches('/').is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

pub(crate) fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k == key {
            return xml::pct_decode(v).or_else(|| Some(v.to_string()));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_query_paths_are_decoded_once_and_keep_sharepoint_percent_names() {
        let decoded = query_param(Some("path=%2F100%25%20complete"), "path").unwrap();
        assert_eq!(
            crate::sanitize_decoded_path(&decoded).unwrap(),
            "/100% complete"
        );
        let traversal = query_param(Some("path=%2F%2e%2e%2Fsecret"), "path").unwrap();
        assert!(crate::sanitize_decoded_path(&traversal).is_err());
    }

    #[test]
    fn query_param_reads_path() {
        assert_eq!(
            query_param(Some("path=/Documents"), "path").as_deref(),
            Some("/Documents")
        );
        assert_eq!(
            query_param(Some("path=%2FDocs%20A"), "path").as_deref(),
            Some("/Docs A")
        );
        assert_eq!(query_param(Some("foo=1"), "path"), None);
        assert_eq!(query_param(None, "path"), None);
    }

    #[test]
    fn join_child_path_normalizes_root() {
        assert_eq!(join_child_path("/", "a"), "/a");
        assert_eq!(join_child_path("/Docs", "a.txt"), "/Docs/a.txt");
    }

    fn item(id: &str, parent: &str, name: &str, is_dir: bool) -> index::DeltaItem {
        index::DeltaItem {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            name: name.to_string(),
            deleted: false,
            is_root: false,
            meta: index::ItemMeta {
                is_dir,
                size: if is_dir { 0 } else { 42 },
                mtime: 1234,
                etag: "etag".to_string(),
            },
        }
    }

    fn root(id: &str) -> index::DeltaItem {
        index::DeltaItem {
            id: id.to_string(),
            parent_id: None,
            name: String::new(),
            deleted: false,
            is_root: true,
            meta: index::ItemMeta {
                is_dir: true,
                size: 0,
                mtime: 0,
                etag: String::new(),
            },
        }
    }

    fn sample_index() -> index::Index {
        let mut idx = index::Index::default();
        idx.apply(root("r"));
        idx.apply(item("docs", "r", "Documents", true));
        idx.apply(item("f1", "docs", "Report.docx", false));
        idx.apply(item("f2", "r", "report-summary.txt", false));
        idx.apply(item("f3", "r", "other.txt", false));
        idx
    }

    #[test]
    fn search_index_requires_min_query_length() {
        let idx = sample_index();
        let out = search_index(&idx, "r");
        assert_eq!(out["ready"], serde_json::json!(true));
        assert_eq!(out["results"], serde_json::json!([]));
    }

    #[test]
    fn search_index_is_case_insensitive_substring_over_files_and_folders() {
        let idx = sample_index();
        let out = search_index(&idx, "REPORT");
        let results = out["results"].as_array().unwrap();
        let names: Vec<&str> = results
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Report.docx"));
        assert!(names.contains(&"report-summary.txt"));
        assert!(!names.contains(&"other.txt"));
    }

    #[test]
    fn search_index_derives_correct_paths_via_parent_walk() {
        let idx = sample_index();
        let out = search_index(&idx, "Report.docx");
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]["path"],
            serde_json::json!("/Documents/Report.docx")
        );
        assert_eq!(results[0]["isDir"], serde_json::json!(false));
    }

    #[test]
    fn search_index_sorts_directories_first_then_by_path() {
        let idx = sample_index();
        let out = search_index(&idx, "do");
        let results = out["results"].as_array().unwrap();
        // "Documents" (dir) should sort before any file matches.
        assert_eq!(results[0]["isDir"], serde_json::json!(true));
    }

    #[test]
    fn search_index_truncates_and_flags_when_over_cap() {
        let mut idx = index::Index::default();
        idx.apply(root("r"));
        for i in 0..(SEARCH_MAX_RESULTS + 5) {
            idx.apply(item(
                &format!("f{i}"),
                "r",
                &format!("match-{i}.txt"),
                false,
            ));
        }
        let out = search_index(&idx, "match");
        assert_eq!(out["truncated"], serde_json::json!(true));
        assert_eq!(out["results"].as_array().unwrap().len(), SEARCH_MAX_RESULTS);
    }

    #[test]
    fn search_index_not_truncated_when_under_cap() {
        let idx = sample_index();
        let out = search_index(&idx, "report");
        assert_eq!(out["truncated"], serde_json::json!(false));
    }

    #[test]
    fn query_param_reads_site_url() {
        assert_eq!(
            query_param(
                Some("siteUrl=https%3A%2F%2Ftenant.sharepoint.com%2Fsites%2FEngineering"),
                "siteUrl"
            )
            .as_deref(),
            Some("https://tenant.sharepoint.com/sites/Engineering")
        );
    }
}
