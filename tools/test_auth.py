import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

import auth_common

spec = importlib.util.spec_from_file_location("browser_login", Path(__file__).with_name("browser-login.py"))
browser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(browser)


class BrowserCallbackTests(unittest.TestCase):
    def setUp(self):
        self.server, self.result, self.done, self.deadline = browser.callback_server("expected", 30)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, path):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{self.server.server_port}{path}", timeout=2) as response:
                return response.status
        except urllib.error.HTTPError as error:
            with error:
                return error.code

    def test_wrong_state_path_and_duplicate_parameters_do_not_complete_callback(self):
        for path in ("/favicon.ico", "/callback?state=wrong&code=bad",
                     "/callback?state=expected&state=expected&code=bad",
                     "/callback?state=expected&code=one&code=two"):
            self.assertEqual(self.request(path), 400)
            self.assertFalse(self.done.is_set())
        self.assertEqual(self.request("/callback?state=expected&code=accepted"), 200)
        self.assertEqual(self.result, {"code": "accepted"})

    def test_callback_is_single_use(self):
        self.assertEqual(self.request("/callback?state=expected&code=one"), 200)
        self.assertEqual(self.request("/callback?state=expected&code=two"), 400)
        self.assertEqual(self.result, {"code": "one"})

    def test_expired_callback_is_rejected(self):
        with patch.object(browser.time, "monotonic", return_value=self.deadline + 1):
            self.assertEqual(self.request("/callback?state=expected&code=late"), 400)
        self.assertFalse(self.done.is_set())

    def test_access_denied_callback_never_exchanges_a_code(self):
        self.assertEqual(self.request("/callback?state=expected&error=access_denied"), 200)
        self.assertEqual(self.result, {"error": "access_denied"})
        self.assertNotIn("code", self.result)

    def test_pkce_matches_rfc7636_fixture(self):
        self.assertEqual(browser.pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
                         "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
        self.assertGreaterEqual(len(browser.pkce_verifier()), 43)


class TokenStoreTests(unittest.TestCase):
    def test_pkce_exchange_failure_is_single_use_and_never_publishes_credentials(self):
        with ThreadPoolExecutor(max_workers=1) as executor:
            callback = []
            challenge = []

            def launch(url):
                query = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
                challenge.append(query["code_challenge"][0])
                redirect = query["redirect_uri"][0]
                callback_url = redirect + "?" + urllib.parse.urlencode({
                    "state": query["state"][0], "code": "fixture-code"})
                def receive():
                    with urllib.request.urlopen(callback_url, timeout=2) as response:
                        return response.status
                callback.append(executor.submit(receive))
                return True

            def exchange(url, fields):
                self.assertEqual(fields["grant_type"], "authorization_code")
                self.assertEqual(fields["code"], "fixture-code")
                self.assertEqual(browser.pkce_challenge(fields["code_verifier"]), challenge[0])
                self.assertTrue(fields["redirect_uri"].endswith("/callback"))
                return {"error": "invalid_grant", "error_description": "private-response"}

            with patch.dict("os.environ", {"ONEDRIVE_CLIENT_ID": "fixture-client",
                            "ONEDRIVE_TENANT_ID": "common", "ONEDRIVE_SCOPES": auth_common.DEFAULT_SCOPES}), \
                 patch.object(browser.webbrowser, "open", side_effect=launch), \
                 patch.object(browser, "form_post", side_effect=exchange) as poster, \
                 patch.object(browser, "write_token") as writer:
                with self.assertRaisesRegex(RuntimeError, "Sign in again"):
                    browser.main()
                poster.assert_called_once()
                writer.assert_not_called()
                self.assertEqual(callback[0].result(), 200)

    def test_interrupted_refresh_is_recovered_before_new_login_replaces_it(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "token.json"
            guest = Path(root) / "guest"
            guest.mkdir()
            (guest / "token.json").write_text('{"refresh_token":"fixture-rotation"}')
            pending = Path(root) / ".pending-token"
            pending.write_text(str(guest / "token.json"))
            auth_common.write_token(path, {"refresh_token": "new-login", "access_token": "fixture"})
            self.assertEqual(json.loads(path.read_text())["refresh_token"], "new-login")
            self.assertFalse(pending.exists())

    def test_new_login_is_private_atomic_and_invalidates_old_login_binding(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "token.json"
            auth_common.write_token(path, {"refresh_token": "fixture-r1", "access_token": "fixture-a1"})
            original = json.loads(path.read_text())
            auth_common.write_token(path, {"refresh_token": "fixture-r2", "access_token": "fixture-a2"})
            updated = json.loads(path.read_text())
            self.assertNotEqual(original["credential_id"], updated["credential_id"])
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            with self.assertRaises(RuntimeError):
                auth_common.write_token(path, {"error": "invalid_grant"})
            self.assertEqual(json.loads(path.read_text()), updated)

    def test_authentication_errors_do_not_echo_payloads(self):
        self.assertNotIn("private", auth_common.auth_error({
            "error": "private-code", "error_description": "private-access-token"}))
        self.assertIn("Conditional access", auth_common.auth_error({
            "error": "invalid_grant", "error_description": "AADSTS53003 private-access-token"}))

    def test_browser_cancellation_closes_listener_without_writing_credentials(self):
        server, _, _, _ = browser.callback_server("unused", 1)
        with patch.dict("os.environ", {"ONEDRIVE_CLIENT_ID": "fixture-client"}), \
             patch.object(browser, "callback_server", return_value=(server, {}, threading.Event(), time.monotonic() + 1)), \
             patch.object(browser.webbrowser, "open", side_effect=KeyboardInterrupt), \
             patch.object(browser, "write_token") as writer:
            with self.assertRaises(KeyboardInterrupt):
                browser.main()
            self.assertEqual(server.socket.fileno(), -1)
            writer.assert_not_called()

    def test_authority_and_scope_validation(self):
        self.assertEqual(auth_common.authority("organizations"), "organizations")
        for value in ("../common", "common?x=1", "tenant/other"):
            with self.assertRaises(ValueError):
                auth_common.authority(value)
        with self.assertRaises(ValueError):
            auth_common.scopes("offline_access Directory.ReadWrite.All")


if __name__ == "__main__":
    unittest.main()
