//! Minimal blocking HTTP/HTTPS client built directly on the `wasi:http`
//! `outgoing-handler` import. TLS termination happens host-side (the
//! `wasi:http` implementation, not this guest) since `rustls`/`ring` do not
//! link into `wasm32-wasip2`.
//!
//! This is the *only* place in the crate that touches the raw
//! `wasi:http/types` resources; `auth.rs` and `graph.rs` call the small
//! `send()` helper below instead of building requests by hand.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use crate::bindings::wasi::io::poll;

pub struct HttpRequest<'a> {
    pub method: Method,
    /// Absolute URL, e.g. `https://graph.microsoft.com/v1.0/me/drive`.
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header_value(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
    }
}

/// Splits an absolute `https://host[:port]/path?query` URL into
/// `(authority, path_with_query)`. Only `https` is supported -- every
/// upstream call this daemon makes (Microsoft Graph, the OAuth token
/// endpoint) is TLS-only.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("only https:// URLs are supported, got: {url}"))?;
    match rest.find('/') {
        Some(idx) => Ok((rest[..idx].to_string(), rest[idx..].to_string())),
        None => Ok((rest.to_string(), "/".to_string())),
    }
}

/// Sends a single request and blocks (via `wasi:io/poll`) until the full
/// response body has been read. Suitable for the small JSON/metadata
/// payloads this daemon deals with; not intended for streaming large file
/// transfers.
pub fn send(req: HttpRequest) -> Result<HttpResponse, String> {
    let (authority, path_with_query) = split_url(req.url)?;

    let headers = Fields::new();
    for (name, value) in req.headers {
        headers
            .append(name, value.as_bytes())
            .map_err(|e| format!("invalid header {name}: {e:?}"))?;
    }

    let request = OutgoingRequest::new(headers);
    request
        .set_method(&req.method)
        .map_err(|_| "invalid HTTP method".to_string())?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|_| "failed to set scheme".to_string())?;
    request
        .set_authority(Some(&authority))
        .map_err(|_| "failed to set authority".to_string())?;
    request
        .set_path_with_query(Some(&path_with_query))
        .map_err(|_| "failed to set path".to_string())?;

    if !req.body.is_empty() {
        let body = request
            .body()
            .map_err(|_| "request body already taken".to_string())?;
        {
            let stream = body
                .write()
                .map_err(|_| "failed to open request body stream".to_string())?;
            stream
                .blocking_write_and_flush(req.body)
                .map_err(|e| format!("failed writing request body: {e:?}"))?;
        }
        OutgoingBody::finish(body, None)
            .map_err(|e| format!("failed finishing request body: {e:?}"))?;
    }

    let future_response = outgoing_handler::handle(request, None)
        .map_err(|e| format!("failed to send request: {e:?}"))?;

    // Block until the response is ready.
    loop {
        if let Some(result) = future_response.get() {
            let response = result
                .map_err(|_| "response already consumed".to_string())?
                .map_err(|e| format!("transport error: {e:?}"))?;
            let status = response.status();
            let headers = read_headers(&response);
            let body = read_body(&response)?;
            return Ok(HttpResponse {
                status,
                headers,
                body,
            });
        }
        let pollable = future_response.subscribe();
        poll::poll(&[&pollable]);
    }
}

fn read_headers(
    response: &crate::bindings::wasi::http::types::IncomingResponse,
) -> Vec<(String, Vec<u8>)> {
    response
        .headers()
        .entries()
        .into_iter()
        .map(|(name, value)| (name, value))
        .collect()
}

fn read_body(
    response: &crate::bindings::wasi::http::types::IncomingResponse,
) -> Result<Vec<u8>, String> {
    let incoming_body = response
        .consume()
        .map_err(|_| "failed to consume response body".to_string())?;
    let stream = incoming_body
        .stream()
        .map_err(|_| "failed to open response body stream".to_string())?;

    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    // A zero-length read with no error means "try again";
                    // give the host a chance to make progress.
                    let pollable = stream.subscribe();
                    poll::poll(&[&pollable]);
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            Err(crate::bindings::wasi::io::streams::StreamError::Closed) => break,
            Err(e) => return Err(format!("error reading response body: {e:?}")),
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_value_is_case_insensitive() {
        let response = HttpResponse {
            status: 302,
            headers: vec![(
                "Location".to_string(),
                b"https://example.test/file".to_vec(),
            )],
            body: Vec::new(),
        };

        assert_eq!(
            response.header_value("location").as_deref(),
            Some("https://example.test/file")
        );
    }
}
