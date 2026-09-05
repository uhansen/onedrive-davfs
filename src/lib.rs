#[allow(warnings)]
mod bindings;

mod auth;
mod config;
mod dav;
mod graph;
mod http_client;
mod state_file;
mod xml;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;

use config::Config;
use dav::DavResponse;

struct Component;

/// Reads the full request body, refusing to buffer more than `max_bytes`
/// so an oversized `PUT` cannot exhaust memory before the size check in
/// `dav::put` runs. Returns `Err(BodyTooLarge)` past the limit.
fn read_request_body(request: &IncomingRequest, max_bytes: u64) -> Result<Vec<u8>, BodyError> {
    let incoming_body = request
        .consume()
        .map_err(|_| BodyError::Read("failed to consume request body".to_string()))?;
    let stream = incoming_body
        .stream()
        .map_err(|_| BodyError::Read("failed to open request body stream".to_string()))?;

    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    let pollable = stream.subscribe();
                    bindings::wasi::io::poll::poll(&[&pollable]);
                    continue;
                }
                if (buf.len() + chunk.len()) as u64 > max_bytes {
                    return Err(BodyError::TooLarge);
                }
                buf.extend_from_slice(&chunk);
            }
            Err(bindings::wasi::io::streams::StreamError::Closed) => break,
            Err(e) => {
                return Err(BodyError::Read(format!(
                    "error reading request body: {e:?}"
                )));
            }
        }
    }
    Ok(buf)
}

enum BodyError {
    TooLarge,
    Read(String),
}

/// Minimal base64 decoder (standard alphabet, `=` padding) -- just enough
/// to check an `Authorization: Basic ...` header without pulling in a
/// crate for it.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let vals: Vec<u8> = chunk.iter().map(|&b| val(b)).collect::<Option<Vec<_>>>()?;
        out.push((vals[0] << 2) | (vals.get(1).copied().unwrap_or(0) >> 4));
        if vals.len() > 2 {
            out.push((vals[1] << 4) | (vals[2] >> 2));
        }
        if vals.len() > 3 {
            out.push((vals[2] << 6) | vals[3]);
        }
    }
    Some(out)
}

/// Defense-in-depth: even though the daemon only binds to loopback,
/// requires a shared-secret `Authorization: Basic` password (configured
/// via `ONEDRIVE_BASIC_AUTH_SECRET` and mirrored into `~/.davfs2/secrets`)
/// so any other local process/user can't silently ride along on the mount.
/// `Config::load` only yields `None` here when the operator explicitly set
/// `ONEDRIVE_ALLOW_UNAUTHENTICATED=1`.
fn check_basic_auth(config: &Config, headers: &Fields) -> bool {
    let Some(expected) = &config.basic_auth_secret else {
        return true;
    };
    let Some(raw) = header_value(headers, "authorization") else {
        return false;
    };
    let Some(encoded) = raw.strip_prefix("Basic ") else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    // "user:password" -- only the password half needs to match the secret.
    match decoded.split_once(':') {
        Some((_, pass)) => constant_time_eq(pass.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

/// Byte-wise comparison whose running time does not depend on where the
/// first mismatch occurs. The length check leaks only the secret's length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Percent-decodes and sanity-checks a request path coming from the WebDAV
/// client. Rejects `.`/`..` segments and control characters so nothing odd
/// is ever forwarded into a Graph `root:/...:` address.
fn sanitize_path(raw: &str) -> Result<String, String> {
    let decoded = xml::pct_decode(raw).ok_or("malformed percent-encoding in path")?;
    if decoded.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("control characters are not allowed in paths".to_string());
    }
    let mut segments = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err("'..' segments are not allowed in paths".to_string()),
            s => segments.push(s),
        }
    }
    if segments.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

fn header_value(headers: &Fields, name: &str) -> Option<String> {
    headers
        .get(name)
        .into_iter()
        .next()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
}

/// Extracts the path portion out of a raw `Destination` header value,
/// which may be an absolute URL (`https://host/OneDrive/foo`) or already a
/// bare path (`/OneDrive/foo`) depending on the WebDAV client.
fn destination_path(raw: &str) -> String {
    if let Some(idx) = raw.find("://") {
        raw[idx + 3..]
            .find('/')
            .map(|i| raw[idx + 3 + i..].to_string())
            .unwrap_or_default()
    } else {
        raw.to_string()
    }
}

fn respond(
    response_out: ResponseOutparam,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&'static str, String)],
) {
    let headers = Fields::new();
    let _ = headers.append("content-type", content_type.as_bytes());
    let _ = headers.append("content-length", body.len().to_string().as_bytes());
    for (name, value) in extra_headers {
        let _ = headers.append(name, value.as_bytes());
    }

    let response = OutgoingResponse::new(headers);
    if response.set_status_code(status).is_err() {
        ResponseOutparam::set(
            response_out,
            Err(ErrorCode::InternalError(Some(
                "failed to set status code".to_string(),
            ))),
        );
        return;
    }

    let outgoing_body = match response.body() {
        Ok(b) => b,
        Err(()) => {
            ResponseOutparam::set(
                response_out,
                Err(ErrorCode::InternalError(Some(
                    "response body already taken".to_string(),
                ))),
            );
            return;
        }
    };

    ResponseOutparam::set(response_out, Ok(response));

    if !body.is_empty() {
        if let Ok(stream) = outgoing_body.write() {
            let pollable = stream.subscribe();
            let mut offset = 0;
            while offset < body.len() {
                bindings::wasi::io::poll::poll(&[&pollable]);
                let permit = match stream.check_write() {
                    Ok(n) => n as usize,
                    Err(StreamError::Closed) => break,
                    Err(StreamError::LastOperationFailed(_)) => break,
                };
                if permit == 0 {
                    continue;
                }
                let end = (offset + permit).min(body.len());
                match stream.write(&body[offset..end]) {
                    Ok(()) => offset = end,
                    Err(StreamError::Closed) => break,
                    Err(StreamError::LastOperationFailed(_)) => break,
                }
            }
            let _ = stream.flush();
            let _ = stream.blocking_flush();
        }
    }
    let _ = OutgoingBody::finish(outgoing_body, None);
}

fn handle_request(request: &IncomingRequest) -> DavResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return DavResponse::error(500, format!("config error: {e}")),
    };

    let raw_path = request
        .path_with_query()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    let headers = request.headers();

    if !check_basic_auth(&config, &headers) {
        return DavResponse::unauthorized();
    }

    let path = match sanitize_path(&raw_path) {
        Ok(p) => p,
        Err(e) => return DavResponse::error(400, e),
    };
    let depth = header_value(&headers, "depth");
    let destination = match header_value(&headers, "destination") {
        Some(d) => match sanitize_path(&destination_path(&d)) {
            Ok(p) => Some(p),
            Err(e) => return DavResponse::error(400, format!("bad Destination: {e}")),
        },
        None => None,
    };

    match request.method() {
        Method::Options => DavResponse::options(),
        Method::Head => dav::head(&config, &path),
        Method::Get => dav::get(&config, &path),
        Method::Put => match read_request_body(request, graph::MAX_SIMPLE_UPLOAD_BYTES) {
            Ok(body) => dav::put(&config, &path, &body),
            Err(BodyError::TooLarge) => DavResponse::error(
                413,
                format!(
                    "file exceeds the {} byte simple-upload limit of this build",
                    graph::MAX_SIMPLE_UPLOAD_BYTES
                ),
            ),
            Err(BodyError::Read(e)) => DavResponse::error(400, e),
        },
        Method::Delete => dav::delete(&config, &path),
        Method::Other(m) => match m.as_str() {
            "PROPFIND" => dav::propfind(&config, &path, depth.as_deref()),
            "MKCOL" => dav::mkcol(&config, &path),
            "MOVE" => match destination {
                Some(dest) => dav::r#move(&config, &path, &dest),
                None => DavResponse::error(400, "missing Destination header"),
            },
            "LOCK" => dav::lock(&config, &path),
            "UNLOCK" => dav::unlock(&config, &path),
            other => DavResponse::error(405, format!("unsupported method: {other}")),
        },
        other => DavResponse::error(405, format!("unsupported method: {other:?}")),
    }
}

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let result = handle_request(&request);
        respond(
            response_out,
            result.status,
            result.content_type,
            &result.body,
            &result.headers,
        );
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn base64_decodes_basic_credentials() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn sanitize_path_decodes_and_normalizes() {
        assert_eq!(sanitize_path("").unwrap(), "/");
        assert_eq!(sanitize_path("/").unwrap(), "/");
        assert_eq!(sanitize_path("/a//b/./c/").unwrap(), "/a/b/c");
        assert_eq!(
            sanitize_path("/Documents/report%201.docx").unwrap(),
            "/Documents/report 1.docx"
        );
        assert_eq!(sanitize_path("/caf%C3%A9").unwrap(), "/café");
    }

    #[test]
    fn sanitize_path_rejects_traversal_and_junk() {
        assert!(sanitize_path("/a/../b").is_err());
        assert!(sanitize_path("/%2e%2e/b").is_err());
        assert!(sanitize_path("/a%00b").is_err());
        assert!(sanitize_path("/a%zz").is_err());
        assert!(sanitize_path("/a\r\nb").is_err());
    }

    #[test]
    fn destination_path_strips_scheme_and_host() {
        assert_eq!(destination_path("http://127.0.0.1:8765/x/y"), "/x/y");
        assert_eq!(destination_path("/x/y"), "/x/y");
        assert_eq!(destination_path("http://host"), "");
    }
}
