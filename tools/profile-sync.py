#!/usr/bin/env python3
"""Authenticated sync tick for one loopback library; no credentials on argv."""

import base64
import os
import sys
import urllib.parse
import urllib.request

from auth_common import NoRedirect

url = sys.argv[1]
parsed = urllib.parse.urlsplit(url)
if parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1", "localhost") or parsed.username or parsed.password:
    raise SystemExit("Sync requires a loopback HTTP endpoint")
secret = os.environ.get("ONEDRIVE_BASIC_AUTH_SECRET")
if not secret:
    raise SystemExit("ONEDRIVE_BASIC_AUTH_SECRET is missing")
request = urllib.request.Request(
    url, method="POST", data=b"",
    headers={"X-OneDrive-Sync": "1", "Authorization":
             "Basic " + base64.b64encode(("daemon:" + secret).encode()).decode()},
)
try:
    with urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect).open(request, timeout=300) as response:
        print(response.read(1024 * 1024).decode())
except OSError:
    raise SystemExit("Library sync failed; check daemon access and service logs") from None
