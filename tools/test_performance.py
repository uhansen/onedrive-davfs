"""Offline regression tests for upload spooling and read/write isolation."""

from concurrent.futures import ThreadPoolExecutor
import fcntl
import os
from pathlib import Path
import socket
import time
import unittest

from benchmark_backend import fixture, request


@unittest.skipUnless(os.environ.get("ONEDRIVE_TEST_HOST") and os.environ.get("ONEDRIVE_TEST_COMPONENT"),
                     "host/component artifacts not selected")
class PerformanceTests(unittest.TestCase):
    def fixture(self, **kwargs):
        return fixture(Path(os.environ["ONEDRIVE_TEST_HOST"]),
                       Path(os.environ["ONEDRIVE_TEST_COMPONENT"]), **kwargs)

    def test_large_and_unknown_length_uploads_use_private_cleaned_spool(self):
        with self.fixture() as (_, port, _, state, _):
            saved = (state / "index.bin").read_bytes()
            block = b"fixture" * 8192
            # Unknown length is fully spooled before authentication fails; no
            # external request can run without the deliberately absent client ID.
            status, body = request(port, "PUT", "/synthetic",
                                   (block for _ in range(1200)), chunked=True)
            self.assertEqual(status, 502)
            self.assertIn(b"missing required env var", body)
            self.assertEqual(list((state / ".uploads").iterdir()), [])
            self.assertEqual(saved, (state / "index.bin").read_bytes())
            self.assertEqual(request(port, "GET", "/_status")[0], 200)

    def test_explicit_upload_limit_does_not_change_snapshot(self):
        with self.fixture(max_upload=1024) as (_, port, _, state, _):
            saved = (state / "index.bin").read_bytes()
            self.assertEqual(request(port, "PUT", "/too-large", b"x" * 1025)[0], 413)
            self.assertEqual(request(port, "PUT", "/unknown", iter([b"x" * 1025]), chunked=True)[0], 413)
            self.assertEqual(list((state / ".uploads").iterdir()), [])
            self.assertEqual(saved, (state / "index.bin").read_bytes())

    def test_disconnected_upload_releases_slot_and_spool_without_cloud_changes(self):
        import base64
        from test_host_component import SECRET
        with self.fixture() as (_, port, _, state, _):
            saved = (state / "index.bin").read_bytes()
            authorization = base64.b64encode(("daemon:" + SECRET).encode()).decode()
            with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                connection.sendall((
                    "PUT /incomplete HTTP/1.1\r\nHost: localhost\r\n"
                    f"Authorization: Basic {authorization}\r\nContent-Length: 1000000\r\n\r\n"
                ).encode() + b"incomplete")
                connection.shutdown(socket.SHUT_WR)
                self.assertIn(b"400", connection.recv(4096))
            self.assertEqual(request(port, "POST", "/_sync")[0], 502)
            self.assertEqual(list((state / ".uploads").iterdir()), [])
            self.assertEqual(saved, (state / "index.bin").read_bytes())

    def test_optional_cached_metadata_does_not_wait_or_publish_stale_account_data(self):
        with self.fixture(cached_reads=True) as (_, port, account, state, log):
            saved = (state / "index.bin").read_bytes()
            with ThreadPoolExecutor(max_workers=1) as pool, (account / ".token.lock").open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                busy = pool.submit(request, port, "POST", "/_sync")
                time.sleep(0.2)
                self.assertFalse(busy.done())
                try:
                    began = time.monotonic()
                    self.assertEqual(request(port, "PROPFIND", "/", headers={"Depth": "1"})[0], 207)
                    self.assertEqual(request(port, "HEAD", "/")[0], 200)
                    self.assertEqual(request(port, "PROPFIND", "/missing", headers={"Depth": "0"})[0], 503)
                    self.assertLess(time.monotonic() - began, 1.5)
                    token = (account / "token.json").read_text()
                    replacement = account / "replacement.json"
                    replacement.write_text(token.replace("fixture-login", "changed-login"))
                    replacement.replace(account / "token.json")
                    self.assertEqual(request(port, "PROPFIND", "/", headers={"Depth": "0"})[0], 503)
                finally:
                    fcntl.flock(lock, fcntl.LOCK_UN)
                self.assertEqual(busy.result()[0], 502)
            self.assertEqual(saved, (state / "index.bin").read_bytes())
            for line in log.read_text().splitlines():
                if "perf" in line:
                    self.assertNotIn("fixture-login", line)
                    self.assertNotIn("CachedFolder", line)
                    self.assertNotIn("fixture-not-real", line)


if __name__ == "__main__":
    unittest.main()
