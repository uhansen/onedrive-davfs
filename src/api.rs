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
    if !config.index_enabled {
        return json_ok(serde_json::json!({
            "ok": true,
            "indexEnabled": false,
        }));
    }

    let meta = match snapshot::read_meta(&config.state_dir) {
        Ok(m) => m,
        Err(e) => {
            return json_ok(serde_json::json!({
                "ok": true,
                "indexEnabled": true,
                "error": e,
            }))
        }
    };
    let last_tick = match sync_stats::load(&config.state_dir) {
        Ok(s) => s,
        Err(_) => None,
    };

    let mut body = serde_json::json!({
        "ok": true,
        "indexEnabled": true,
        "present": meta.is_some(),
    });
    if let Some(meta) = meta {
        body["crawlComplete"] = serde_json::json!(meta.crawl_complete);
        body["hasToken"] = serde_json::json!(meta.delta_token.is_some());
        body["generation"] = serde_json::json!(meta.generation);
        body["builtAt"] = serde_json::json!(meta.built_at);
        body["totalDirs"] = serde_json::json!(meta.total_dirs);
        body["totalFiles"] = serde_json::json!(meta.total_files);
        body["pendingNextLink"] = serde_json::json!(meta.pending_next_link.is_some());
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

pub fn tree(config: &Config, query: Option<&str>) -> DavResponse {
    let raw = query_param(query, "path").unwrap_or_else(|| "/".to_string());
    let path = match crate::sanitize_path(&raw) {
        Ok(p) => p,
        Err(e) => return DavResponse::error(400, e),
    };

    if config.index_enabled {
        match snapshot::lookup_file(&config.state_dir, &path) {
            Ok(Some(hit)) => return json_ok(lookup_to_tree(&path, hit, "index")),
            Ok(None) => {}
            Err(_) => {}
        }
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
}
