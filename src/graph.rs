//! Thin Microsoft Graph client. Every call goes through `http_client::send`
//! (which itself goes through `wasi:http/outgoing-handler`) and attaches a
//! fresh bearer token from `auth::bearer_token`.
//!
//! Paths are POSIX-style (`/Documents/report.docx`, `""` for the drive
//! root) and are translated to Graph item URLs under either a drive id
//! (`/drives/{drive-id}/root:/path/to/item:`) or a selector such as
//! `/me/drive/root:/path/to/item:`.

use serde::Deserialize;

use crate::bindings::wasi::http::types::Method;
use crate::config::Config;
use crate::http_client::{self, HttpRequest};

/// Graph's simple (non-chunked) upload only supports files up to ~4 MiB.
/// Anything bigger needs a resumable upload session, which is explicitly
/// out of scope for this first pass.
pub const MAX_SIMPLE_UPLOAD_BYTES: u64 = 4 * 1024 * 1024;

pub struct GraphItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds.
    pub last_modified: u64,
    pub etag: String,
}

#[derive(Deserialize)]
struct DriveItem {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    folder: Option<serde_json::Value>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified: Option<String>,
    #[serde(rename = "eTag", default)]
    etag: Option<String>,
}

#[derive(Deserialize)]
struct DriveItemPage {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

/// Converts an ISO-8601 Graph timestamp (`"2026-01-12T08:30:00Z"`) to Unix
/// seconds. Deliberately tolerant: any parse failure returns `0` rather
/// than failing the whole PROPFIND response over one bad timestamp.
fn parse_iso8601_to_unix(s: &str) -> u64 {
    let digits: Vec<u32> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    let [year, month, day, hour, minute, second, ..] = digits[..] else {
        return 0;
    };
    let days = days_from_civil(year as i64, month, day);
    (days * 86400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64).max(0) as u64
}

/// Inverse of `xml::civil_from_days` (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn drive_root_segment(drive_base: &str) -> String {
    let trimmed = drive_base.trim_matches('/');
    if trimmed.contains('/') {
        format!("/{trimmed}")
    } else {
        format!("/drives/{trimmed}")
    }
}

/// Graph `parentReference.path` uses `/<drive>/root:` for the drive root
/// and `/<drive>/root:/folder` (no trailing colon) for a folder — not the
/// item-address form (`.../root:/folder:`) used in request URLs.
fn parent_reference_path(drive_base: &str, parent: &str) -> String {
    let drive_root = drive_root_segment(drive_base);
    let trimmed = parent.trim_matches('/');
    if trimmed.is_empty() {
        format!("{drive_root}/root:")
    } else {
        format!(
            "{drive_root}/root:/{}",
            trimmed
                .split('/')
                .map(crate::xml::pct_encode)
                .collect::<Vec<_>>()
                .join("/")
        )
    }
}

fn item_path_segment(drive_base: &str, path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let drive_root = drive_root_segment(drive_base);
    if trimmed.is_empty() {
        format!("{drive_root}/root")
    } else {
        format!(
            "{drive_root}/root:/{}:",
            trimmed
                .split('/')
                .map(crate::xml::pct_encode)
                .collect::<Vec<_>>()
                .join("/")
        )
    }
}

fn graph_request(
    config: &Config,
    method: Method,
    url: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<http_client::HttpResponse, String> {
    let bearer = crate::auth::bearer_token(config)?;
    let mut headers = vec![("authorization", bearer.as_str())];
    if let Some(ct) = content_type {
        headers.push(("content-type", ct));
    }
    http_client::send(HttpRequest {
        method,
        url,
        headers: &headers,
        body,
    })
}

fn to_item(item: &DriveItem) -> GraphItem {
    GraphItem {
        name: item.name.clone(),
        is_dir: item.folder.is_some(),
        size: item.size,
        last_modified: item
            .last_modified
            .as_deref()
            .map(parse_iso8601_to_unix)
            .unwrap_or(0),
        etag: item.etag.clone().unwrap_or_default(),
    }
}

/// Metadata for a single item (used to answer `PROPFIND Depth: 0`).
pub fn stat(config: &Config, path: &str) -> Result<GraphItem, String> {
    let url = format!(
        "https://graph.microsoft.com/v1.0{}",
        item_path_segment(&config.drive_base, path)
    );
    let response = graph_request(config, Method::Get, &url, None, &[])?;
    if response.status == 404 {
        return Err("not found".to_string());
    }
    if response.status != 200 {
        return Err(format!(
            "graph stat failed with status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ));
    }
    let item: DriveItem = serde_json::from_slice(&response.body)
        .map_err(|e| format!("failed to parse drive item: {e}"))?;
    Ok(to_item(&item))
}

/// Lists the immediate children of a folder, following
/// `@odata.nextLink` pagination (Graph pages at ~200 items).
pub fn children(config: &Config, path: &str) -> Result<Vec<GraphItem>, String> {
    let mut url = format!(
        "https://graph.microsoft.com/v1.0{}/children?$top=200",
        item_path_segment(&config.drive_base, path)
    );
    let mut items = Vec::new();
    let mut pages = 0usize;
    loop {
        let response = graph_request(config, Method::Get, &url, None, &[])?;
        if response.status != 200 {
            return Err(format!(
                "graph children failed with status {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ));
        }
        let page: DriveItemPage = serde_json::from_slice(&response.body)
            .map_err(|e| format!("failed to parse children page: {e}"))?;
        items.extend(page.value.iter().map(to_item));
        pages += 1;
        match page.next_link {
            Some(next) => url = validate_next_link(&next, pages)?,
            None => break,
        }
    }
    Ok(items)
}

/// Upper bound on pagination round-trips for a single listing (200 items
/// per page => 100k entries), guarding against a runaway/cyclic link chain.
const MAX_CHILDREN_PAGES: usize = 500;

/// Only ever follow pagination links back to Graph itself: the bearer token
/// is attached to every `graph_request`, so a link pointing anywhere else
/// would leak it.
fn validate_next_link(next: &str, pages_so_far: usize) -> Result<String, String> {
    if pages_so_far >= MAX_CHILDREN_PAGES {
        return Err(format!(
            "graph children listing exceeded {MAX_CHILDREN_PAGES} pages; aborting"
        ));
    }
    if !next.starts_with(GRAPH_ORIGIN) {
        return Err(
            "graph @odata.nextLink points outside graph.microsoft.com; refusing".to_string(),
        );
    }
    Ok(next.to_string())
}

const GRAPH_ORIGIN: &str = "https://graph.microsoft.com/";

/// Hosts Graph uses for pre-authenticated `/content` download redirects.
/// Matched exactly or as a DNS suffix (so `foo.sharepoint.com` is allowed
/// but `sharepoint.com.evil.example` is not). Bearer is never attached.
fn is_allowed_download_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let hostport = rest.split('/').next().unwrap_or("");
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostport
            .rsplit_once(':')
            .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
            .map(|(h, _)| h)
            .unwrap_or(hostport)
    };
    let host = host.to_ascii_lowercase();
    const ALLOWED: &[&str] = &[
        "graph.microsoft.com",
        "sharepoint.com",
        "sharepointonline.com",
        "onedrive.com",
        "live.com",
        "live.net",
        "1drv.com",
        "1drv.ms",
        "office.com",
        "office365.com",
        "blob.core.windows.net",
    ];
    ALLOWED
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

pub fn get_content(config: &Config, path: &str) -> Result<Vec<u8>, String> {
    let url = format!(
        "https://graph.microsoft.com/v1.0{}/content",
        item_path_segment(&config.drive_base, path)
    );
    let response = graph_request(config, Method::Get, &url, None, &[])?;
    match response.status {
        200 => Ok(response.body),
        301 | 302 | 303 | 307 | 308 => {
            let location = response
                .header_value("location")
                .ok_or_else(|| "graph get_content redirect missing Location header".to_string())?;
            if !is_allowed_download_url(&location) {
                return Err(format!(
                    "graph get_content redirect to unsupported URL: {location}"
                ));
            }

            let redirected = http_client::send(HttpRequest {
                method: Method::Get,
                url: &location,
                headers: &[],
                body: &[],
            })?;
            if redirected.status != 200 {
                return Err(format!(
                    "graph redirected get_content failed with status {}",
                    redirected.status
                ));
            }
            Ok(redirected.body)
        }
        _ => Err(format!(
            "graph get_content failed with status {}",
            response.status
        )),
    }
}

/// Direct (simple) upload -- callers must check `MAX_SIMPLE_UPLOAD_BYTES`
/// first; larger files are rejected explicitly rather than silently
/// truncated (chunked/resumable upload sessions are a follow-up).
pub fn put_content(config: &Config, path: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > MAX_SIMPLE_UPLOAD_BYTES {
        return Err(format!(
            "file is {} bytes, larger than this build's {}-byte simple upload limit \
             (chunked upload sessions are not implemented yet)",
            bytes.len(),
            MAX_SIMPLE_UPLOAD_BYTES
        ));
    }
    let url = format!(
        "https://graph.microsoft.com/v1.0{}/content",
        item_path_segment(&config.drive_base, path)
    );
    let response = graph_request(
        config,
        Method::Put,
        &url,
        Some("application/octet-stream"),
        bytes,
    )?;
    if response.status != 200 && response.status != 201 {
        return Err(format!(
            "graph put_content failed with status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ));
    }
    Ok(())
}

pub fn create_folder(config: &Config, parent_path: &str, name: &str) -> Result<(), String> {
    let url = format!(
        "https://graph.microsoft.com/v1.0{}/children",
        item_path_segment(&config.drive_base, parent_path)
    );
    let body = serde_json::json!({
        "name": name,
        "folder": {},
        "@microsoft.graph.conflictBehavior": "fail",
    });
    let response = graph_request(
        config,
        Method::Post,
        &url,
        Some("application/json"),
        body.to_string().as_bytes(),
    )?;
    if response.status != 201 {
        return Err(format!(
            "graph create_folder failed with status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ));
    }
    Ok(())
}

pub fn delete(config: &Config, path: &str) -> Result<(), String> {
    let url = format!(
        "https://graph.microsoft.com/v1.0{}",
        item_path_segment(&config.drive_base, path)
    );
    let response = graph_request(config, Method::Delete, &url, None, &[])?;
    if response.status != 204 && response.status != 404 {
        return Err(format!(
            "graph delete failed with status {}",
            response.status
        ));
    }
    Ok(())
}

/// Handles both rename-in-place and move-to-another-folder, since a
/// WebDAV `MOVE` covers both (`dav.rs` just passes the destination path
/// through).
pub fn move_or_rename(config: &Config, from_path: &str, to_path: &str) -> Result<(), String> {
    let url = format!(
        "https://graph.microsoft.com/v1.0{}",
        item_path_segment(&config.drive_base, from_path)
    );
    let new_name = to_path.rsplit('/').next().unwrap_or(to_path);
    let new_parent = match to_path.rfind('/') {
        Some(idx) => &to_path[..idx],
        None => "",
    };
    let body = serde_json::json!({
        "name": new_name,
        "parentReference": {
            "path": parent_reference_path(&config.drive_base, new_parent)
        },
    });
    let response = graph_request(
        config,
        Method::Patch,
        &url,
        Some("application/json"),
        body.to_string().as_bytes(),
    )?;
    if response.status != 200 {
        return Err(format!(
            "graph move_or_rename failed with status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601_utc() {
        assert_eq!(parse_iso8601_to_unix("2026-01-12T08:30:00Z"), 1768206600);
    }

    #[test]
    fn parses_iso8601_with_fractional_seconds() {
        assert_eq!(
            parse_iso8601_to_unix("2026-01-12T08:30:00.1234567Z"),
            1768206600
        );
    }

    #[test]
    fn tolerates_garbage_by_returning_zero() {
        assert_eq!(parse_iso8601_to_unix("not-a-date"), 0);
    }

    #[test]
    fn days_from_civil_matches_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 1, 12), 20465);
    }

    #[test]
    fn drive_root_segment_supports_default_me_drive_selector() {
        assert_eq!(drive_root_segment("me/drive"), "/me/drive");
    }

    #[test]
    fn drive_root_segment_supports_explicit_drive_id() {
        assert_eq!(
            drive_root_segment("B087983F641B9ED3"),
            "/drives/B087983F641B9ED3"
        );
    }

    #[test]
    fn item_path_segment_preserves_graph_selector_shape() {
        assert_eq!(item_path_segment("me/drive", ""), "/me/drive/root");
        assert_eq!(
            item_path_segment("me/drive", "/Documents/report 1.docx"),
            "/me/drive/root:/Documents/report%201.docx:"
        );
    }

    #[test]
    fn next_link_must_stay_on_graph_origin() {
        assert!(validate_next_link("https://graph.microsoft.com/v1.0/x?skip=1", 1).is_ok());
        assert!(validate_next_link("https://evil.example/steal", 1).is_err());
        assert!(validate_next_link("http://graph.microsoft.com/v1.0/x", 1).is_err());
        assert!(validate_next_link("https://graph.microsoft.com.evil/x", 1).is_err());
        assert!(
            validate_next_link("https://graph.microsoft.com/v1.0/x", MAX_CHILDREN_PAGES).is_err()
        );
    }

    #[test]
    fn download_redirect_hosts_are_pinned() {
        assert!(is_allowed_download_url(
            "https://mytenant.sharepoint.com/sites/x/_layouts/download.aspx?x=1"
        ));
        assert!(is_allowed_download_url(
            "https://graph.microsoft.com/v1.0/me/drive/items/id/content"
        ));
        assert!(is_allowed_download_url(
            "https://bn.files.1drv.com/y4mABC/file.bin"
        ));
        assert!(!is_allowed_download_url("http://sharepoint.com/x"));
        assert!(!is_allowed_download_url("https://evil.example/x"));
        assert!(!is_allowed_download_url(
            "https://sharepoint.com.evil.example/x"
        ));
        assert!(!is_allowed_download_url("https://notsharepoint.com/x"));
    }

    #[test]
    fn move_parent_reference_uses_drive_selector() {
        assert_eq!(
            parent_reference_path("me/drive", "/Documents"),
            "/me/drive/root:/Documents"
        );
        assert_eq!(parent_reference_path("me/drive", "/"), "/me/drive/root:");
        assert_eq!(
            parent_reference_path("B087983F641B9ED3", "/Documents"),
            "/drives/B087983F641B9ED3/root:/Documents"
        );
    }
}
