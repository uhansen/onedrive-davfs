#!/usr/bin/env python3
"""Offline host/guest benchmark. Uses disposable credentials; never calls Graph."""

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import fcntl
import http.client
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time

from test_host_component import SECRET, snapshot


@contextmanager
def fixture(host, component, cached_reads=False, max_upload=1024 * 1024 * 1024):
    with tempfile.TemporaryDirectory(prefix="onedrive-performance-") as directory:
        root = Path(directory)
        account = root / "account"
        state = root / "state"
        account.mkdir()
        state.mkdir()
        (state / ".uploads").mkdir(mode=0o700)
        (account / "token.json").write_text(json.dumps({
            "credential_id": "fixture-login", "refresh_token": "fixture-not-real",
            "access_token": None, "expires_at": 0,
        }))
        (state / "index.bin").write_bytes(snapshot("drives/b!fixture", "fixture-login"))
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        environment = {key: value for key, value in os.environ.items() if not key.startswith("ONEDRIVE_")}
        environment.update(
            ONEDRIVE_BASIC_AUTH_SECRET=SECRET, ONEDRIVE_DRIVE_BASE="drives/b!fixture",
            ONEDRIVE_MAX_UPLOAD_BYTES=str(max_upload),
            ONEDRIVE_CACHED_DAV_READS="1" if cached_reads else "0",
            ONEDRIVE_PERF_LOG="1",
        )
        log = (root / "host.log").open("w+")
        process = subprocess.Popen([
            str(host), "--component", str(component), "--state-dir", str(state),
            "--account-dir", str(account), "--addr", f"127.0.0.1:{port}",
        ], env=environment, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    log.seek(0)
                    raise RuntimeError("fixture failed: " + log.read())
                try:
                    if request(port, "GET", "/_status")[0] == 200:
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                raise RuntimeError("fixture startup timed out")
            yield process, port, account, state, root / "host.log"
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)
            log.close()


def request(port, method, path, body=None, headers=None, chunked=False):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    auth = base64.b64encode(("daemon:" + SECRET).encode()).decode()
    fields = {"Authorization": "Basic " + auth}
    if path == "/_sync":
        fields["X-OneDrive-Sync"] = "1"
    else:
        fields["X-OneDrive-Plugin"] = "1"
    fields.update(headers or {})
    try:
        connection.request(method, path, body, fields, encode_chunked=chunked)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def benchmark(host, component, cached_reads=False, large_upload=False, upload_mib=65):
    result = {}
    with fixture(host, component, cached_reads) as (process, port, account, state, _):
        samples = []
        for _ in range(25):
            began = time.monotonic()
            assert request(port, "GET", "/_status")[0] == 200
            samples.append((time.monotonic() - began) * 1000)
        result["status_median_ms"] = round(statistics.median(samples), 3)
        result["status_p95_ms"] = round(sorted(samples)[23], 3)
        # A held credential lock deterministically models a slow writer without
        # contacting Microsoft or modifying a running account.
        with ThreadPoolExecutor(max_workers=2) as pool, (account / ".token.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            busy = pool.submit(request, port, "POST", "/_sync")
            time.sleep(0.2)
            assert not busy.done()
            def release():
                time.sleep(0.5)
                fcntl.flock(lock, fcntl.LOCK_UN)
            released = pool.submit(release)
            began = time.monotonic()
            assert request(port, "PROPFIND", "/", headers={"Depth": "1"})[0] == 207
            result["metadata_during_writer_ms"] = round((time.monotonic() - began) * 1000, 3)
            released.result()
            assert busy.result()[0] == 502
        if large_upload:
            began = time.monotonic()
            chunk = b"x" * (1024 * 1024)
            status, body = request(port, "PUT", "/synthetic-upload", (chunk for _ in range(upload_mib)),
                                   chunked=True)
            assert status == 502 and b"missing required env var" in body, (status, body)
            assert not list(state.glob(".upload-*"))
            result["upload_mib"] = upload_mib
            result["spool_seconds"] = round(time.monotonic() - began, 3)
            result["host_peak_rss_kib"] = int(next(
                line.split()[1] for line in Path(f"/proc/{process.pid}/status").read_text().splitlines()
                if line.startswith("VmHWM:")
            ))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=Path, required=True)
    parser.add_argument("--component", type=Path, required=True)
    parser.add_argument("--cached-dav-reads", action="store_true")
    parser.add_argument("--large-upload", action="store_true")
    parser.add_argument("--upload-mib", type=int, default=65)
    args = parser.parse_args()
    if not 1 <= args.upload_mib <= 128:
        parser.error("--upload-mib must be between 1 and 128")
    print(json.dumps(benchmark(args.host.resolve(), args.component.resolve(),
                               args.cached_dav_reads, args.large_upload, args.upload_mib), indent=2))


if __name__ == "__main__":
    main()
