"""Offline host/guest integration; set both ONEDRIVE_TEST_* artifact paths to run."""

import base64
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request

SECRET = "F!xture-Host-Component-Only-123456789"


def string(value):
    encoded = value.encode()
    return struct.pack("<I", len(encoded)) + encoded


def snapshot(drive, credential):
    binding = {"account_id": "fixture-account", "credential_id": credential, "client_id": None,
               "tenant_id": "common", "drive_base": drive, "drive_id": drive.split("/")[1],
               "drive_name": "Fixture Documents", "drive_type": "documentLibrary",
               "web_url": "", "owner_label": ""}
    child = string("CachedFolder") + b"\x01" + struct.pack("<QQ", 0, 0) + string("") + string("child")
    row = string("/") + struct.pack("<QI", 32, len(child)) + string("root") + struct.pack("<QQ", 0, 0) + string("")
    meta = json.dumps({"binding": binding, "delta_token": "https://graph.microsoft.com/v1.0/fixture/delta?token=old",
                       "pending_next_link": None, "crawl_complete": True, "generation": 1,
                       "built_at": 1, "root_id": "root", "total_dirs": 2, "total_files": 0}).encode()
    return struct.pack("<4sIQIQI", b"ODIX", 1, 32 + len(child), 1, 32 + len(child) + len(row), len(meta)) + child + row + meta


@unittest.skipUnless(os.environ.get("ONEDRIVE_TEST_HOST") and os.environ.get("ONEDRIVE_TEST_COMPONENT"),
                     "host/component artifacts not selected")
class HostComponentTests(unittest.TestCase):
    def test_shared_account_mounts_switch_without_exposing_old_cached_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            account = root / "account"
            account.mkdir()
            token = {"credential_id": "fixture-login-one", "refresh_token": "fixture-not-real",
                     "access_token": None, "expires_at": 0}
            (account / "token.json").write_text(json.dumps(token))
            hosts = []

            def stop(process):
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)

            def start(state, drive):
                with socket.socket() as probe:
                    probe.bind(("127.0.0.1", 0))
                    port = probe.getsockname()[1]
                environment = {key: value for key, value in os.environ.items() if not key.startswith("ONEDRIVE_")}
                environment.update(ONEDRIVE_BASIC_AUTH_SECRET=SECRET, ONEDRIVE_DRIVE_BASE=drive)
                # No client ID or usable bearer: any attempted Graph fallback fails before network I/O.
                process = subprocess.Popen([
                    os.environ["ONEDRIVE_TEST_HOST"], "--component", os.environ["ONEDRIVE_TEST_COMPONENT"],
                    "--state-dir", str(state), "--account-dir", str(account), "--addr", f"127.0.0.1:{port}",
                ], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                hosts.append(process)
                url = f"http://127.0.0.1:{port}"
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        self.fail("Fixture host exited before becoming ready")
                    try:
                        request(url, "/_status")
                        return process, url
                    except urllib.error.URLError:
                        time.sleep(0.05)
                self.fail("Fixture host did not become ready")

            def request(url, endpoint, sync=False):
                req = urllib.request.Request(url + endpoint, method="POST" if sync else "GET", headers={
                    "Authorization": "Basic " + base64.b64encode(("daemon:" + SECRET).encode()).decode(),
                    "X-OneDrive-Sync" if sync else "X-OneDrive-Plugin": "1",
                })
                with urllib.request.urlopen(req, timeout=10) as response:
                    return json.load(response)

            try:
                urls = []
                for number in (1, 2):
                    state = root / f"library{number}"
                    state.mkdir()
                    drive = f"drives/b!fixture{number}"
                    (state / "index.bin").write_bytes(snapshot(drive, token["credential_id"]))
                    _, url = start(state, drive)
                    urls.append(url)
                    self.assertTrue(request(url, "/_status")["accessVerified"])
                    self.assertEqual(request(url, "/_search?q=Cached")["results"][0]["path"], "/CachedFolder")
                    self.assertEqual(request(url, "/_tree?path=/")["children"][0]["name"], "CachedFolder")
                stop(hosts[0])
                self.assertTrue(request(urls[1], "/_status")["accessVerified"])
                _, urls[0] = start(root / "library1", "drives/b!fixture1")
                with ThreadPoolExecutor(max_workers=1) as pool, (account / ".token.lock").open("a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                    busy = pool.submit(request, urls[0], "/_sync", True)
                    time.sleep(0.2)
                    self.assertFalse(busy.done())
                    local_token = root / "library1/token.json"
                    token_mtime = local_token.stat().st_mtime_ns
                    saved_index = (root / "library1/index.bin").read_bytes()
                    began = time.monotonic()
                    self.assertTrue(request(urls[0], "/_status")["accessVerified"])
                    self.assertEqual(request(urls[0], "/_tree?path=/")["source"], "index")
                    self.assertTrue(request(urls[0], "/_search?q=Cached")["results"])
                    missing = request(urls[0], "/_tree?path=/MissingFolder")
                    self.assertFalse(missing["ok"])
                    self.assertIn("index", missing["error"])
                    with self.assertRaises(urllib.error.HTTPError) as probe:
                        urllib.request.urlopen(urls[0] + "/", timeout=1.5)
                    self.assertEqual(probe.exception.code, 401)
                    probe.exception.close()
                    self.assertLess(time.monotonic() - began, 1.5,
                                    "local reads must not wait for credential/Graph operations")
                    self.assertEqual(token_mtime, local_token.stat().st_mtime_ns)
                    self.assertEqual(saved_index, (root / "library1/index.bin").read_bytes())
                    original = (account / "token.json").read_bytes()
                    replacement = account / "new-login.json"
                    replacement.write_text(json.dumps(dict(token, credential_id="interrupted-login")))
                    replacement.replace(account / "token.json")
                    with self.assertRaises(urllib.error.HTTPError) as gated:
                        request(urls[0], "/_search?q=Cached")
                    self.assertEqual(gated.exception.code, 503)
                    gated.exception.close()
                    replacement.write_bytes(original)
                    replacement.replace(account / "token.json")
                    fcntl.flock(lock, fcntl.LOCK_UN)
                    with self.assertRaises(urllib.error.HTTPError) as failed_tick:
                        busy.result(timeout=10)
                    failed_tick.exception.close()
                before = (root / "library1/index.bin").read_bytes()
                token["credential_id"] = "fixture-login-two"
                temporary = account / "replacement.json"
                temporary.write_text(json.dumps(token))
                temporary.replace(account / "token.json")
                for url in urls:
                    status = request(url, "/_status")
                    self.assertFalse(status["accessVerified"])
                    self.assertNotIn("drive", status)
                    self.assertFalse(request(url, "/_search?q=Cached")["ready"])
                with self.assertRaises(urllib.error.HTTPError) as failure:
                    request(urls[0], "/_sync", sync=True)
                failure.exception.close()
                self.assertIn("missing required env var", request(urls[0], "/_status")["lastSyncError"])
                self.assertEqual(before, (root / "library1/index.bin").read_bytes(),
                                 "failed verification must preserve the old snapshot for rollback")
            finally:
                for process in hosts:
                    if process.poll() is None:
                        stop(process)


if __name__ == "__main__":
    unittest.main()
