import json
from pathlib import Path
import tempfile
import unittest
import sys
from unittest.mock import patch

import profiles


class ProfileSetupTests(unittest.TestCase):
    def test_source_scopes_match_account_as_sets_and_reject_wider_consent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential_dir = root / "account"
            credential_dir.mkdir()
            (credential_dir / "token.json").write_text('{"credential_id":"fixture-login"}')
            account = {"id": "work", "clientId": "fixture", "tenantId": "common",
                       "scopes": profiles.DEFAULT_SCOPES, "credentialDir": str(credential_dir)}
            registry_path = root / "profiles.json"
            registry_path.write_text(json.dumps({"version": 1, "accounts": [account], "libraries": []}))
            before = registry_path.read_bytes()
            drive = {"id": "b!fixture", "accountId": "fixture-user", "tenantId": "common",
                     "credentialId": "fixture-login"}
            args = ["profiles.py", "--registry", str(registry_path), "library", "--account", "work",
                    "--source-url", "http://127.0.0.1:8790/", "--source-env", "fixture.env",
                    "--selector", "me/drive", "--name", "Documents"]
            reordered = " ".join(reversed(profiles.DEFAULT_SCOPES.split())) + " offline_access"
            for source_scopes in (None, reordered, profiles.DEFAULT_SCOPES + " Sites.Read.All"):
                with self.subTest(scopes=source_scopes):
                    values = {"ONEDRIVE_CLIENT_ID": "fixture", "ONEDRIVE_TENANT_ID": "common"}
                    if source_scopes is not None:
                        values["ONEDRIVE_SCOPES"] = source_scopes
                    with patch.object(sys, "argv", args), \
                            patch.object(profiles, "resolve_target", return_value=(drive, values)), \
                            patch.object(profiles, "create_library", return_value={}) as create, \
                            patch.object(profiles, "available_port", return_value=8801), \
                            patch("builtins.print"):
                        if source_scopes and "Sites.Read.All" in source_scopes:
                            with self.assertRaisesRegex(ValueError, "same delegated scopes"):
                                profiles.main()
                            create.assert_not_called()
                        else:
                            profiles.main()
                            create.assert_called_once()
                    self.assertEqual(before, registry_path.read_bytes())

    def test_source_from_another_login_cannot_publish_a_library(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential_dir = root / "account"
            credential_dir.mkdir()
            (credential_dir / "token.json").write_text('{"credential_id":"login-one"}')
            account = {"id": "work", "clientId": "fixture", "tenantId": "common",
                       "scopes": profiles.DEFAULT_SCOPES, "credentialDir": str(credential_dir)}
            registry_path = root / "profiles.json"
            registry_path.write_text(json.dumps({"version": 1, "accounts": [account], "libraries": []}))
            before = registry_path.read_bytes()
            drive = {"id": "b!fixture", "accountId": "fixture-user", "tenantId": "common",
                     "credentialId": "different-login"}
            args = ["profiles.py", "--registry", str(registry_path), "library", "--account", "work",
                    "--source-url", "http://127.0.0.1:8790/", "--source-env", "fixture.env",
                    "--selector", "me/drive", "--name", "Documents"]
            with patch.object(sys, "argv", args), patch.object(profiles, "resolve_target", return_value=(drive, {})):
                with self.assertRaisesRegex(ValueError, "login session"):
                    profiles.main()
            self.assertEqual(before, registry_path.read_bytes())
            self.assertFalse((root / "library-data").exists())

    def test_publication_is_isolated_and_duplicate_target_does_not_change_registry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            registry_path = root / "profiles.json"
            registry = {"version": 1, "accounts": [], "libraries": []}
            account = {"id": "work", "clientId": "fixture-client", "tenantId": "fixture-tenant",
                       "scopes": "offline_access Files.ReadWrite.All User.Read",
                       "credentialDir": str(root / "account")}
            account2 = dict(account, id="personal", credentialDir=str(root / "account2"))
            drive = {"id": "b!fixture-library", "accountId": "fixture-user"}
            profiles.create_library(registry_path, registry, account, drive, "Documents",
                                    "workdocs", root / "mount1", 8790)
            profiles.create_library(registry_path, registry, account2, drive, "Documents",
                                    "personal", root / "mount2", 8791)
            installed = json.loads(registry_path.read_text())
            self.assertNotEqual(installed["libraries"][0]["stateDir"], installed["libraries"][1]["stateDir"])
            first = root / "library-data/workdocs"
            self.assertIn("--account-dir", (first / "onedrive-davfs-workdocs.service").read_text())
            self.assertIn(account["credentialDir"], (first / "onedrive-davfs-workdocs.service").read_text())
            self.assertEqual((first / "env").stat().st_mode & 0o777, 0o600)
            before = registry_path.read_bytes()
            with self.assertRaises(ValueError):
                profiles.create_library(registry_path, registry, account, drive, "Duplicate",
                                        "duplicate", root / "mount3", 8792)
            self.assertEqual(before, registry_path.read_bytes())

    def test_failed_registry_publish_rolls_back_new_profile_without_touching_old_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "profiles.json"
            registry = {"version": 1, "accounts": [], "libraries": []}
            account = {"id": "work", "clientId": "fixture", "tenantId": "common",
                       "scopes": profiles.DEFAULT_SCOPES, "credentialDir": str(root / "account")}
            with patch.object(profiles, "publish_registry", side_effect=OSError("fixture failure")):
                with self.assertRaises(OSError):
                    profiles.create_library(path, registry, account, {"id": "fixture"}, "Docs",
                                            "docs", root / "mount", 8790)
            self.assertFalse((root / "library-data/docs").exists())
            self.assertFalse(path.exists())
            self.assertEqual(registry["libraries"], [])

    def test_unit_arguments_do_not_interpolate_shell_or_systemd_specifiers(self):
        self.assertEqual(profiles.systemd_arg("/fixture/100% docs"), '"/fixture/100%% docs"')
        self.assertEqual(profiles.systemd_arg("/fixture/${HOME}"), '"/fixture/$${HOME}"')
        self.assertEqual(profiles.systemd_arg("/fixture/${HOME}", escape_environment=False),
                         '"/fixture/${HOME}"')
        with self.assertRaises(ValueError):
            profiles.systemd_arg("/fixture\nExecStart=/bin/false")
        with self.assertRaises(ValueError):
            profiles.identifier("../another")


if __name__ == "__main__":
    unittest.main()
