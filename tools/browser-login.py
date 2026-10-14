#!/usr/bin/env python3
"""Interactive OAuth bootstrap for onedrive-davfs using auth-code + PKCE.

This runs natively, outside the Wasm sandbox, opens the user's browser to the
Microsoft sign-in page, listens on a loopback callback, exchanges the single-
use authorization code, and writes token.json with owner-only permissions.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import secrets
import socket
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


DEFAULT_SCOPES = "offline_access Files.ReadWrite.All User.Read"
DEFAULT_TIMEOUT = 300


@dataclass
class CallbackResult:
    code: str | None = None
    state: str | None = None
    error: str | None = None
    error_description: str | None = None


def form_post(url: str, fields: list[tuple[str, str]]) -> dict:
    body = urllib.parse.urlencode(fields).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        payload = exc.read().decode("utf-8", errors="replace") if exc.fp else ""
        raise RuntimeError(f"HTTP {exc.code}: {payload[:400]}") from exc
    return json.loads(payload)


def pkce_verifier() -> str:
    return base64.urlsafe_b64encode(secrets.token_bytes(32)).decode("ascii").rstrip("=")


def pkce_challenge(verifier: str) -> str:
    digest = hashlib.sha256(verifier.encode("utf-8")).digest()
    return base64.urlsafe_b64encode(digest).decode("ascii").rstrip("=")


def install_secure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path, 0o700)


def write_token(path: Path, payload: dict) -> None:
    out = {
        "refresh_token": payload["refresh_token"],
        "access_token": payload["access_token"],
        "expires_at": int(time.time()) + int(payload.get("expires_in", 3600)) - 60,
    }
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(out, handle, indent=2)
    os.chmod(path, 0o600)


def callback_server(timeout: int) -> tuple[HTTPServer, CallbackResult, threading.Event]:
    result = CallbackResult()
    done = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            parsed = urllib.parse.urlparse(self.path)
            query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
            result.code = query.get("code", [None])[0]
            result.state = query.get("state", [None])[0]
            result.error = query.get("error", [None])[0]
            result.error_description = query.get("error_description", [None])[0]
            done.set()
            body = (
                "<html><body><h1>OneDrive sign-in complete</h1>"
                "<p>You can close this tab and return to the terminal.</p>"
                "</body></html>"
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: object) -> None:  # noqa: A003
            return

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    host, port = sock.getsockname()
    sock.close()
    server = HTTPServer((host, port), Handler)
    server.timeout = 0.5
    return server, result, done


def main() -> int:
    state_dir = Path(os.path.expanduser(os.environ.get("ONEDRIVE_STATE_DIR", "~/.local/state/onedrive-davfs")))
    tenant_id = os.environ.get("ONEDRIVE_TENANT_ID", "common")
    client_id = os.environ.get("ONEDRIVE_CLIENT_ID")
    if not client_id:
        raise SystemExit("Set ONEDRIVE_CLIENT_ID to your Azure AD app application (client) ID")
    scopes = os.environ.get("ONEDRIVE_SCOPES", DEFAULT_SCOPES).split()
    if not scopes:
        raise SystemExit("ONEDRIVE_SCOPES must contain at least one scope")
    timeout = int(os.environ.get("ONEDRIVE_BROWSER_LOGIN_TIMEOUT_SECS", str(DEFAULT_TIMEOUT)))

    install_secure_dir(state_dir)
    token_path = state_dir / "token.json"
    verifier = pkce_verifier()
    challenge = pkce_challenge(verifier)
    state = secrets.token_urlsafe(24)

    server, result, done = callback_server(timeout)
    redirect_uri = f"http://127.0.0.1:{server.server_port}/callback"
    auth_url = (
        f"https://login.microsoftonline.com/{urllib.parse.quote(tenant_id, safe='')}/oauth2/v2.0/authorize?"
        + urllib.parse.urlencode(
            [
                ("client_id", client_id),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri),
                ("response_mode", "query"),
                ("scope", " ".join(scopes)),
                ("state", state),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ]
        )
    )

    print("Opening browser for OneDrive sign-in...", flush=True)
    if not webbrowser.open(auth_url):
        print("Could not open the browser automatically. Open this URL manually:", flush=True)
        print(auth_url, flush=True)

    deadline = time.time() + timeout
    while time.time() < deadline and not done.is_set():
        server.handle_request()
    server.server_close()

    if not done.is_set():
        raise SystemExit("Timed out waiting for the browser sign-in callback")
    if result.state != state:
        raise SystemExit("Rejected callback with mismatched OAuth state")
    if result.error:
        detail = result.error_description or result.error
        raise SystemExit(f"Browser sign-in failed: {detail}")
    if not result.code:
        raise SystemExit("Browser sign-in finished without an authorization code")

    token_url = f"https://login.microsoftonline.com/{urllib.parse.quote(tenant_id, safe='')}/oauth2/v2.0/token"
    token = form_post(
        token_url,
        [
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", result.code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", " ".join(scopes)),
        ],
    )
    if "refresh_token" not in token or "access_token" not in token:
        raise SystemExit(f"Token response missing expected fields: {json.dumps(token)[:400]}")

    write_token(token_path, token)
    print(f"Wrote {token_path}. The onedrive-davfs daemon can now refresh from this token.", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
