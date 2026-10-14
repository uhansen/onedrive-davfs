"""Native authentication utilities shared by browser and device login."""

import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import secrets
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

DEFAULT_SCOPES = "offline_access Files.ReadWrite.All User.Read"
ALLOWED_SCOPES = {"offline_access", "Files.ReadWrite.All", "User.Read", "Sites.Read.All"}


def authority(value):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.-]*", value):
        raise ValueError("Invalid tenant authority; use a tenant id/domain or common/organizations/consumers")
    return value


def scopes(value):
    result = list(dict.fromkeys(value.split()))
    if not result or not set(result).issubset(ALLOWED_SCOPES):
        raise ValueError("OAuth scopes must be offline_access, Files.ReadWrite.All, User.Read, or Sites.Read.All")
    if "offline_access" not in result:
        raise ValueError("offline_access is required to obtain a refresh token")
    return " ".join(result)


def auth_error(payload):
    description = payload.get("error_description", "")
    if isinstance(description, str) and "AADSTS530" in description:
        return "Conditional access or device-compliance policy requires interactive sign-in or administrator action."
    code = payload.get("error", "unknown_error")
    known = {
        "invalid_grant": "Sign in again; the credential is expired or revoked.",
        "consent_required": "Administrator consent or browser sign-in is required.",
        "interaction_required": "Browser sign-in or tenant policy requires user interaction.",
        "access_denied": "Access denied by the user or tenant policy.",
        "invalid_client": "Check the public-client app registration and client ID.",
        "authorization_pending": "Waiting for sign-in.",
        "expired_token": "Sign-in expired; start again.",
        "slow_down": "The authorization server requested slower polling.",
    }
    return known.get(code, "Authentication failed; check app registration, tenant policy, and scopes.") if isinstance(code, str) else "Authentication server returned an invalid error response."


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def form_post(url, fields):
    request = urllib.request.Request(
        url,
        data=urllib.parse.urlencode(fields).encode("utf-8"),
        headers={"Content-Type": "application/x-www-form-urlencoded"},
        method="POST",
    )
    try:
        response = urllib.request.build_opener(NoRedirect).open(request, timeout=30)
    except urllib.error.HTTPError as exc:
        response = exc
    with response:
        data = response.read(1024 * 1024 + 1)
        if len(data) > 1024 * 1024:
            raise RuntimeError("Authentication response exceeded the size limit")
        try:
            payload = json.loads(data)
        except ValueError:
            raise RuntimeError("Authentication server returned invalid JSON") from None
        if not isinstance(payload, dict):
            raise RuntimeError("Authentication server returned an unexpected response")
        if response.status != 200 and "error" not in payload:
            raise RuntimeError(f"Authentication server returned HTTP {response.status}")
        return payload


@contextlib.contextmanager
def credential_lock(directory, timeout=30):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(directory, 0o700)
    fd = os.open(directory / ".token.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    deadline = time.monotonic() + timeout
    try:
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("Timed out waiting for the account credential lock")
                time.sleep(0.05)
        yield
    finally:
        os.close(fd)


def atomic_private_write(path, data):
    fd, temporary = tempfile.mkstemp(prefix=".token-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def write_token(path, payload):
    if not isinstance(payload.get("refresh_token"), str) or not payload["refresh_token"]:
        raise RuntimeError("Authentication response did not contain a refresh token")
    if not isinstance(payload.get("access_token"), str) or not payload["access_token"]:
        raise RuntimeError("Authentication response did not contain an access token")
    try:
        expires_in = int(payload.get("expires_in", 3600))
    except (TypeError, ValueError):
        raise RuntimeError("Authentication server returned an invalid token expiry") from None
    out = {
        "credential_id": secrets.token_hex(32),
        "refresh_token": payload["refresh_token"],
        "access_token": payload["access_token"],
        "expires_at": int(time.time()) + max(0, expires_in - 60),
    }
    path = Path(path)
    with credential_lock(path.parent):
        pending = path.parent / ".pending-token"
        if pending.exists():
            interrupted = Path(pending.read_text())
            if not interrupted.is_absolute() or interrupted.name != "token.json":
                raise RuntimeError("Interrupted credential handoff has an invalid recovery path")
            atomic_private_write(path, interrupted.read_bytes())
            pending.unlink()
        atomic_private_write(path, json.dumps(out).encode("utf-8"))
