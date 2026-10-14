#!/usr/bin/env python3
"""Native device-code login using the same scope/store policy as PKCE."""

import os
from pathlib import Path
import sys
import time

from auth_common import DEFAULT_SCOPES, auth_error, authority, form_post, scopes, write_token


def main():
    directory = Path(os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else
        os.environ.get("ONEDRIVE_ACCOUNT_DIR",
                       os.environ.get("ONEDRIVE_STATE_DIR", "~/.local/state/onedrive-davfs"))))
    tenant = authority(sys.argv[2] if len(sys.argv) > 2 else os.environ.get("ONEDRIVE_TENANT_ID", "common"))
    client = os.environ.get("ONEDRIVE_CLIENT_ID")
    if not client:
        raise ValueError("Set ONEDRIVE_CLIENT_ID to your public-client application ID")
    scope = scopes(os.environ.get("ONEDRIVE_SCOPES", DEFAULT_SCOPES))
    base = f"https://login.microsoftonline.com/{tenant}/oauth2/v2.0/"
    device = form_post(base + "devicecode", {"client_id": client, "scope": scope})
    if "error" in device:
        raise RuntimeError(auth_error(device))
    uri = device["verification_uri"]
    if uri not in ("https://microsoft.com/devicelogin", "https://www.microsoft.com/link",
                   "https://login.microsoftonline.com/common/oauth2/deviceauth"):
        raise RuntimeError("Unexpected device verification URL")
    print(f"Open: {uri}\nCode: {device['user_code']}", flush=True)
    interval = max(1, int(device.get("interval", 5)))
    deadline = time.monotonic() + min(1800, int(device.get("expires_in", 900)))
    while time.monotonic() < deadline:
        time.sleep(min(interval, max(0, deadline - time.monotonic())))
        if time.monotonic() >= deadline:
            break
        token = form_post(base + "token", {
            "client_id": client,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": device["device_code"],
        })
        error = token.get("error")
        if error == "authorization_pending":
            continue
        if error == "slow_down":
            interval += 5
            continue
        if error:
            raise RuntimeError(auth_error(token))
        write_token(directory / "token.json", token)
        print("Sign-in complete; account credentials saved.", flush=True)
        return 0
    raise TimeoutError("Device-code sign-in expired; start again")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit("Sign-in canceled")
    except (ValueError, RuntimeError, OSError) as error:
        raise SystemExit(str(error))
