//! WebDAV verb handlers. Each function takes already-extracted request
//! parts (path, depth, destination, body bytes) and returns
//! `(status_code, content_type, body)` -- `lib.rs` is the only place that
//! touches the raw `wasi:http` request/response resources.

use crate::config::Config;
use crate::graph;
use crate::index::{self, DeltaItem, ItemMeta};
use crate::snapshot::{self, ChildInfo, Lookup};
use crate::xml::{self, DavEntry};

pub struct DavResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
    /// When set, sent as `Content-Length` instead of `body.len()` (HEAD).
    pub content_length: Option<u64>,
}

impl DavResponse {
    fn xml(status: u16, body: String) -> Self {
        DavResponse {
            status,
            content_type: "application/xml; charset=utf-8".to_string(),
            body: body.into_bytes(),
            headers: Vec::new(),
            content_length: None,
        }
    }

    fn empty(status: u16) -> Self {
        DavResponse {
            status,
            content_type: "text/plain".to_string(),
            body: Vec::new(),
            headers: Vec::new(),
            content_length: None,
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        DavResponse {
            status,
            content_type: "text/plain".to_string(),
            body: message.into().into_bytes(),
            headers: Vec::new(),
            content_length: None,
        }
    }

    pub fn unauthorized() -> Self {
        DavResponse {
            status: 401,
            content_type: "text/plain".to_string(),
            body: b"unauthorized".to_vec(),
            headers: vec![(
                "www-authenticate",
                r#"Basic realm="onedrive-davfs""#.to_string(),
            )],
            content_length: None,
        }
    }

    pub fn options() -> Self {
        DavResponse {
            status: 200,
            content_type: "text/plain".to_string(),
            body: Vec::new(),
            headers: vec![
                (
                    "allow",
                    "OPTIONS, PROPFIND, GET, PUT, MKCOL, DELETE, MOVE, LOCK, UNLOCK".to_string(),
                ),
                ("dav", "1, 2".to_string()),
                ("ms-author-via", "DAV".to_string()),
            ],
            content_length: None,
        }
    }
}

fn to_entry(path: &str, item: &graph::GraphItem) -> DavEntry {
    let href = if path.trim_matches('/').is_empty() {
        format!("/{}", item.name)
    } else {
        format!("{}/{}", path.trim_end_matches('/'), item.name)
    };
    DavEntry {
        href,
        is_dir: item.is_dir,
        size: item.size,
        last_modified: item.last_modified,
        etag: item.etag.clone(),
    }
}

/// `depth` is the raw `Depth` header value (`"0"`, `"1"`, `"infinity"`, or
/// absent). Per RFC 4918, `infinity` is refused with `403` +
/// `propfind-finite-depth` -- Graph has no cheap way to answer an
/// unbounded recursive listing, and davfs2 never actually sends it anyway.
pub fn propfind(config: &Config, path: &str, depth: Option<&str>) -> DavResponse {
    if depth == Some("infinity") {
        return DavResponse::xml(
            403,
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#
                .to_string(),
        );
    }

    if let Some(resp) = propfind_from_index(config, path, depth) {
        return resp;
    }

    let self_item = match graph::stat(config, path) {
        Ok(item) => item,
        Err(e) if e == "not found" => return DavResponse::empty(404),
        Err(e) => return DavResponse::error(502, e),
    };

    // The drive root is named "root" by Graph; its href must still be "/".
    let mut self_entry = to_entry(&parent_of(path), &self_item);
    if path.trim_matches('/').is_empty() {
        self_entry.href = "/".to_string();
    }
    let mut entries = vec![self_entry];

    if self_item.is_dir && depth != Some("0") {
        match graph::children(config, path) {
            Ok(children) => {
                entries.extend(children.iter().map(|c| to_entry(path, c)));
            }
            Err(e) => return DavResponse::error(502, e),
        }
    }

    DavResponse::xml(207, xml::multistatus(&entries))
}

fn propfind_from_index(config: &Config, path: &str, depth: Option<&str>) -> Option<DavResponse> {
    if !config.index_enabled {
        return None;
    }
    let hit = match snapshot::lookup_file(&config.state_dir, path) {
        Ok(Some(h)) => h,
        _ => return None,
    };
    let entries = match hit {
        Lookup::Dir { info, children } => {
            let mut entries = vec![entry_from_info(path, &info, true)];
            if depth != Some("0") {
                entries.extend(children.iter().map(|c| entry_from_info(path, c, false)));
            }
            entries
        }
        Lookup::File(info) => vec![entry_from_info(path, &info, true)],
    };
    Some(DavResponse::xml(207, xml::multistatus(&entries)))
}

fn entry_from_info(path: &str, info: &ChildInfo, is_self: bool) -> DavEntry {
    let href = if is_self {
        if path.trim_matches('/').is_empty() {
            "/".to_string()
        } else {
            index::normalize_path(path)
        }
    } else if path.trim_matches('/').is_empty() {
        format!("/{}", info.name)
    } else {
        format!("{}/{}", path.trim_end_matches('/'), info.name)
    };
    DavEntry {
        href,
        is_dir: info.is_dir,
        size: info.size,
        last_modified: info.mtime,
        etag: info.etag.clone(),
    }
}

fn parent_of(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => String::new(),
    }
}

pub fn get(config: &Config, path: &str) -> DavResponse {
    match graph::get_content(config, path) {
        Ok(bytes) => DavResponse {
            status: 200,
            content_type: "application/octet-stream".to_string(),
            body: bytes,
            headers: Vec::new(),
            content_length: None,
        },
        Err(e) => DavResponse::error(502, e),
    }
}

pub fn head(config: &Config, path: &str) -> DavResponse {
    if let Some(resp) = head_from_index(config, path) {
        return resp;
    }
    match graph::stat(config, path) {
        Ok(item) => {
            let content_type = if item.is_dir {
                "httpd/unix-directory".to_string()
            } else if item.mime_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                item.mime_type
            };
            DavResponse {
                status: 200,
                content_type,
                body: Vec::new(),
                headers: Vec::new(),
                content_length: Some(if item.is_dir { 0 } else { item.size }),
            }
        }
        Err(e) if e == "not found" => DavResponse::empty(404),
        Err(e) => DavResponse::error(502, e),
    }
}

fn head_from_index(config: &Config, path: &str) -> Option<DavResponse> {
    if !config.index_enabled {
        return None;
    }
    let hit = match snapshot::lookup_file(&config.state_dir, path) {
        Ok(Some(h)) => h,
        _ => return None,
    };
    let (is_dir, size) = match hit {
        Lookup::Dir { info, .. } => (true, info.size),
        Lookup::File(info) => (false, info.size),
    };
    Some(DavResponse {
        status: 200,
        content_type: if is_dir {
            "httpd/unix-directory".to_string()
        } else {
            "application/octet-stream".to_string()
        },
        body: Vec::new(),
        headers: Vec::new(),
        content_length: Some(if is_dir { 0 } else { size }),
    })
}

fn write_through(config: &Config, f: impl FnOnce(&mut crate::index::Index) -> bool) {
    if !config.index_enabled {
        return;
    }
    snapshot::mutate(&config.state_dir, f);
}

fn apply_or_upsert(
    idx: &mut crate::index::Index,
    item: Option<DeltaItem>,
    path: &str,
    meta: ItemMeta,
) -> bool {
    if let Some(it) = item {
        idx.apply(it);
        if idx.resolve(path).is_some() {
            return true;
        }
    }
    idx.upsert_at_path(path, meta)
}

pub fn put(config: &Config, path: &str, body: &[u8]) -> DavResponse {
    if body.len() as u64 > graph::MAX_UPLOAD_BYTES {
        return DavResponse::error(
            413,
            format!(
                "file is {} bytes; this build only supports uploads up to {} bytes",
                body.len(),
                graph::MAX_UPLOAD_BYTES
            ),
        );
    }
    match graph::put_content(config, path, body) {
        Ok(item) => {
            write_through(config, |idx| {
                apply_or_upsert(
                    idx,
                    item,
                    path,
                    ItemMeta {
                        is_dir: false,
                        size: body.len() as u64,
                        mtime: 0,
                        etag: String::new(),
                    },
                )
            });
            DavResponse::empty(201)
        }
        Err(e) => DavResponse::error(502, e),
    }
}

pub fn mkcol(config: &Config, path: &str) -> DavResponse {
    let parent = parent_of(path);
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    match graph::create_folder(config, &parent, name) {
        Ok(item) => {
            write_through(config, |idx| {
                apply_or_upsert(
                    idx,
                    item,
                    path,
                    ItemMeta {
                        is_dir: true,
                        size: 0,
                        mtime: 0,
                        etag: String::new(),
                    },
                )
            });
            DavResponse::empty(201)
        }
        Err(e) => DavResponse::error(502, e),
    }
}

pub fn delete(config: &Config, path: &str) -> DavResponse {
    match graph::delete(config, path) {
        Ok(()) => {
            write_through(config, |idx| idx.remove_at_path(path));
            DavResponse::empty(204)
        }
        Err(e) => DavResponse::error(502, e),
    }
}

/// `destination` is the decoded path portion of the `Destination` header
/// (host/scheme already stripped by `lib.rs`).
pub fn r#move(config: &Config, path: &str, destination: &str) -> DavResponse {
    match graph::move_or_rename(config, path, destination) {
        Ok(item) => {
            write_through(config, |idx| {
                if let Some(it) = item {
                    idx.apply(it);
                    if idx.resolve(destination).is_some() {
                        return true;
                    }
                }
                idx.move_at_path(path, destination)
            });
            DavResponse::empty(201)
        }
        Err(e) => DavResponse::error(502, e),
    }
}

/// davfs2 requires `LOCK` to succeed to allow writes, but Graph has no
/// native locking concept worth modeling for a single-writer mount, so
/// this is a fixed no-op success response rather than a real lock grant.
pub fn lock(_config: &Config, path: &str) -> DavResponse {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:prop xmlns:D="DAV:">
  <D:lockdiscovery>
    <D:activelock>
      <D:locktype><D:write/></D:locktype>
      <D:lockscope><D:exclusive/></D:lockscope>
      <D:depth>0</D:depth>
      <D:owner>onedrive-davfs</D:owner>
      <D:timeout>Second-3600</D:timeout>
      <D:locktoken><D:href>opaquelocktoken:{token}</D:href></D:locktoken>
      <D:lockroot><D:href>{href}</D:href></D:lockroot>
    </D:activelock>
  </D:lockdiscovery>
</D:prop>"#,
        token = xml::pct_encode(path),
        href = xml::pct_encode(path),
    );
    let mut response = DavResponse::xml(200, body);
    response.headers.push((
        "lock-token",
        format!("<opaquelocktoken:{token}>", token = xml::pct_encode(path)),
    ));
    response
}

pub fn unlock(_config: &Config, _path: &str) -> DavResponse {
    DavResponse::empty(204)
}
