#!/usr/bin/env python3
"""Native authorization-code/PKCE login; tokens never enter frontend code."""

import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import os
from pathlib import Path
import secrets
import threading
import time
import urllib.parse
import webbrowser

from auth_common import DEFAULT_SCOPES, auth_error, authority, form_post, scopes, write_token

DEFAULT_TIMEOUT = 300


def pkce_verifier():
    return secrets.token_urlsafe(32)


def pkce_challenge(verifier):
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode("ascii")).digest()).decode("ascii").rstrip("=")


def callback_server(expected_state, timeout=DEFAULT_TIMEOUT):
    if not 1 <= timeout <= 900:
        raise ValueError("Browser callback timeout must be between 1 and 900 seconds")
    result = {}
    done = threading.Event()
    deadline = time.monotonic() + timeout

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            self.request.settimeout(1)
            super().setup()

        def do_GET(self):
            parsed = urllib.parse.urlsplit(self.path)
            query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
            accepted = (
                parsed.path == "/callback"
                and time.monotonic() < deadline
                and not done.is_set()
                and len(query.get("state", [])) == 1
                and secrets.compare_digest(query["state"][0], expected_state)
                and ((len(query.get("code", [])) == 1 and query["code"][0] and "error" not in query)
                     or (len(query.get("error", [])) == 1 and "code" not in query))
            )
            if accepted:
                result.update({key: query[key][0] for key in ("code", "error") if key in query})
                done.set()
            body = (
                b"Sign-in callback received. Return to the terminal to complete authentication."
                if accepted else b"Rejected callback."
            )
            self.send_response(200 if accepted else 400)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    server.timeout = 0.25
    return server, result, done, deadline


def main():
    state_dir = Path(os.path.expanduser(os.environ.get("ONEDRIVE_ACCOUNT_DIR",
                    os.environ.get("ONEDRIVE_STATE_DIR", "~/.local/state/onedrive-davfs"))))
    tenant = authority(os.environ.get("ONEDRIVE_TENANT_ID", "common"))
    client = os.environ.get("ONEDRIVE_CLIENT_ID")
    if not client:
        raise ValueError("Set ONEDRIVE_CLIENT_ID to your public-client application ID")
    scope = scopes(os.environ.get("ONEDRIVE_SCOPES", DEFAULT_SCOPES))
    timeout = int(os.environ.get("ONEDRIVE_BROWSER_LOGIN_TIMEOUT_SECS", str(DEFAULT_TIMEOUT)))
    verifier = pkce_verifier()
    state = secrets.token_urlsafe(32)
    server, result, done, deadline = callback_server(state, timeout)
    redirect = f"http://localhost:{server.server_port}/callback"
    url = f"https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize?" + urllib.parse.urlencode({
        "client_id": client,
        "response_type": "code",
        "redirect_uri": redirect,
        "response_mode": "query",
        "scope": scope,
        "state": state,
        "code_challenge": pkce_challenge(verifier),
        "code_challenge_method": "S256",
    })
    try:
        print("Opening browser for OneDrive sign-in...", flush=True)
        if not webbrowser.open(url):
            raise RuntimeError("Could not open the browser; use device-code login instead")
        while time.monotonic() < deadline and not done.is_set():
            server.handle_request()
    finally:
        server.server_close()
    if not done.is_set():
        raise TimeoutError("Browser sign-in expired; start again")
    if "error" in result:
        raise RuntimeError(auth_error(result))
    token = form_post(f"https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token", {
        "client_id": client,
        "grant_type": "authorization_code",
        "code": result.pop("code"),
        "redirect_uri": redirect,
        "code_verifier": verifier,
        "scope": scope,
    })
    if "error" in token:
        raise RuntimeError(auth_error(token))
    write_token(state_dir / "token.json", token)
    print("Sign-in complete; account credentials saved.", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit("Sign-in canceled")
    except (ValueError, RuntimeError, OSError) as error:
        raise SystemExit(str(error))
