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

use config::Config;
use dav::DavResponse;

struct Component;

/// Reads the full request body. WebDAV request bodies handled by this
/// daemon (`PUT` of files under the simple-upload ceiling, and small
/// `PROPFIND`/`LOCK` XML bodies) are always small, so a single blocking
/// read-to-end is fine.
fn read_request_body(request: &IncomingRequest) -> Result<Vec<u8>, String> {
    let incoming_body = request
        .consume()
        .map_err(|_| "failed to consume request body".to_string())?;
    let stream = incoming_body
        .stream()
        .map_err(|_| "failed to open request body stream".to_string())?;

    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    let pollable = stream.subscribe();
                    bindings::wasi::io::poll::poll(&[&pollable]);
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            Err(bindings::wasi::io::streams::StreamError::Closed) => break,
            Err(e) => return Err(format!("error reading request body: {e:?}")),
        }
    }
    Ok(buf)
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
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
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
/// When no secret is configured, auth is skipped (useful for local dev).
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
    decoded.split_once(':').map(|(_, pass)| pass) == Some(expected.as_str())
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

fn respond(response_out: ResponseOutparam, status: u16, content_type: &str, body: &[u8]) {
    let headers = Fields::new();
    let _ = headers.append("content-type", content_type.as_bytes());
    let _ = headers.append("content-length", body.len().to_string().as_bytes());

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
            let _ = stream.blocking_write_and_flush(body);
        }
    }
    let _ = OutgoingBody::finish(outgoing_body, None);
}

fn handle_request(request: &IncomingRequest) -> DavResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return DavResponse::error(500, format!("config error: {e}")),
    };

    let path = request
        .path_with_query()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    let headers = request.headers();

    if !check_basic_auth(&config, &headers) {
        return DavResponse::error(401, "unauthorized");
    }

    let depth = header_value(&headers, "depth");
    let destination = header_value(&headers, "destination").map(|d| destination_path(&d));

    match request.method() {
        Method::Get => dav::get(&config, &path),
        Method::Put => match read_request_body(request) {
            Ok(body) => dav::put(&config, &path, &body),
            Err(e) => DavResponse::error(400, e),
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
        );
    }
}

bindings::export!(Component with_types_in bindings);
