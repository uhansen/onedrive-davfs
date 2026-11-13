#!/usr/bin/env python3
"""Native account/library setup. Publishes config; never starts services."""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import socket
import tempfile
import urllib.error
import urllib.parse
import urllib.request

from auth_common import DEFAULT_SCOPES, NoRedirect, authority, credential_lock, scopes

ID_PATTERN = r"[A-Za-z0-9_-]{1,64}"


def identifier(value):
    if not re.fullmatch(ID_PATTERN, value):
        raise ValueError("Profile IDs must contain 1-64 letters, digits, underscores, or hyphens")
    return value


def read_registry(path):
    if not path.exists():
        return {"version": 1, "accounts": [], "libraries": []}
    data = json.loads(path.read_text())
    if not isinstance(data, dict) or data.get("version") != 1:
        raise ValueError("Unsupported profile registry")
    if not isinstance(data.get("accounts"), list) or not isinstance(data.get("libraries"), list):
        raise ValueError("Invalid account/library registry")
    return data


def publish_registry(path, document):
    fd, temporary = tempfile.mkstemp(prefix=".profiles-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(document, handle, indent=2)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def env_values(path):
    result = {}
    for raw in Path(os.path.expanduser(path)).read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        result[key.strip()] = value.strip().strip("\"'")
    return result


def resolve_target(source, selector):
    url = urllib.parse.urlsplit(source["url"])
    if (url.scheme != "http" or url.hostname not in ("127.0.0.1", "localhost", "::1")
            or url.username or url.password or url.path not in ("", "/") or url.query or url.fragment):
        raise ValueError("Discovery source must be a loopback HTTP root")
    values = env_values(source["envFile"])
    secret = values.get("ONEDRIVE_BASIC_AUTH_SECRET")
    if not secret:
        raise ValueError("Discovery source lacks a WebDAV secret")
    request = urllib.request.Request(
        source["url"].rstrip("/") + "/_resolve?" + urllib.parse.urlencode({"selector": selector}),
        headers={"X-OneDrive-Plugin": "1",
                 "Authorization": "Basic " + base64.b64encode(("daemon:" + secret).encode()).decode()},
    )
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect).open(request, timeout=60) as response:
            data = response.read(1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"Library verification failed (HTTP {error.code}); check account access and consent") from None
    if len(data) > 1024 * 1024:
        raise ValueError("Library verification response exceeded the size limit")
    payload = json.loads(data)
    drive = payload.get("drive") if payload.get("ok") else None
    if not isinstance(drive, dict) or not drive.get("id") or not drive.get("accountId"):
        raise ValueError("Library verification returned no canonical drive/account identity")
    return drive, values


def available_port(libraries):
    used = {urllib.parse.urlsplit(library["url"]).port for library in libraries}
    for port in range(8790, 8890):
        if port in used:
            continue
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                continue
        return port
    raise RuntimeError("No unused loopback endpoint is available in ports 8790-8889")


def systemd_arg(value, *, escape_environment=True):
    if any(char in str(value) for char in "\r\n\x00"):
        raise ValueError("Unsupported control character in systemd argument")
    value = str(value).replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
    if escape_environment:
        value = value.replace("$", "$$")
    return '"' + value + '"'


def create_library(registry_path, registry, account, drive, name, library_id, mount_point, port):
    identifier(library_id)
    if len(registry["libraries"]) >= 32:
        raise ValueError("At most 32 library profiles are supported")
    state_root = registry_path.parent / "library-data"
    final = state_root / library_id
    state_dir = final / "state"
    mount_point = Path(mount_point).expanduser().resolve()
    account_dir = Path(account["credentialDir"]).resolve()
    for first, second in ((mount_point, state_dir), (mount_point, account_dir), (state_dir, account_dir)):
        if os.path.commonpath((first, second)) in (str(first), str(second)):
            raise ValueError("Mount, library state, and account credentials must be separate directories")
    url = f"http://127.0.0.1:{port}/"
    for library in registry["libraries"]:
        if library["id"] == library_id or urllib.parse.urlsplit(library["url"]).port == port:
            raise ValueError("Library ID or endpoint is already in use")
        if library["accountId"] == account["id"] and library.get("driveId") == drive["id"]:
            raise ValueError("This account/library is already configured")
        for candidate in (mount_point, state_dir):
            for key in ("mountPoint", "stateDir"):
                previous = Path(library[key]).resolve()
                if os.path.commonpath((candidate, previous)) in (str(candidate), str(previous)):
                    raise ValueError("Library mountpoints/state directories must not overlap")
    if final.exists() or mount_point == Path("/") or mount_point.exists():
        raise ValueError("Choose a new, non-root profile directory and mountpoint")
    state_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = Path(tempfile.mkdtemp(prefix=".library-", dir=state_root))
    profile = {
        "id": library_id, "accountId": account["id"], "name": name,
        "driveId": drive["id"], "selector": "drives/" + drive["id"],
        "mountPoint": str(mount_point), "stateDir": str(state_dir),
        "envFile": str(final / "env"), "url": url,
        "daemonServiceUnit": f"onedrive-davfs-{library_id}.service",
        "mountServiceUnit": f"onedrive-davfs-{library_id}-mount.service",
        "enabled": True,
        "credentialDir": account["credentialDir"],
    }
    try:
        (temporary / "state").mkdir(mode=0o700)
        secret = secrets.token_urlsafe(32)
        environment = {
            "ONEDRIVE_CLIENT_ID": account["clientId"],
            "ONEDRIVE_TENANT_ID": account["tenantId"],
            "ONEDRIVE_SCOPES": account["scopes"],
            "ONEDRIVE_DRIVE_BASE": profile["selector"],
            "ONEDRIVE_BASIC_AUTH_SECRET": secret,
        }
        for value in environment.values():
            if any(char in value for char in "\r\n\x00\"'\\"):
                raise ValueError("Unsupported environment value")
        (temporary / "env").write_text("".join(f'{key}="{value}"\n' for key, value in environment.items()))
        (temporary / "env").chmod(0o600)
        home = Path.home()
        host = home / ".local/bin/onedrive-davfs-host"
        component = home / ".local/share/onedrive-davfs/onedrive_davfs.wasm"
        profile["reconnectCommand"] = " ".join(
            shlex.quote(value) for value in (
                "env", "ONEDRIVE_CLIENT_ID=" + account["clientId"],
                "ONEDRIVE_TENANT_ID=" + account["tenantId"],
                "ONEDRIVE_SCOPES=" + account["scopes"],
                "ONEDRIVE_ACCOUNT_DIR=" + account["credentialDir"],
                "python3", str(home / ".local/share/onedrive-davfs/tools/browser-login.py"),
            ))
        command = " ".join(systemd_arg(value) for value in (
            host, "--addr", f"127.0.0.1:{port}", "--state-dir", state_dir,
            "--account-dir", account["credentialDir"], "--component", component))
        daemon = (
            "[Unit]\nDescription=OneDrive library host\nAfter=network-online.target\n"
            "[Service]\nType=exec\nUMask=0077\nNoNewPrivileges=yes\n"
            f"EnvironmentFile={systemd_arg(profile['envFile'], escape_environment=False)}\nExecStart={command}\n"
            "Restart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n")
        (temporary / profile["daemonServiceUnit"]).write_text(daemon)
        mount = (
            "[Unit]\nDescription=OneDrive library davfs mount\n"
            f"After={profile['daemonServiceUnit']}\nRequires={profile['daemonServiceUnit']}\n"
            "[Service]\nType=forking\n"
            f"ExecStart=/usr/bin/mount {systemd_arg(mount_point)}\n"
            f"ExecStop=/usr/bin/umount {systemd_arg(mount_point)}\n"
            "Restart=on-failure\nRestartSec=5\n"
            "[Install]\nWantedBy=default.target\n")
        (temporary / profile["mountServiceUnit"]).write_text(mount)
        sync_unit = f"onedrive-davfs-{library_id}-sync.service"
        sync = (
            f"[Unit]\nAfter={profile['daemonServiceUnit']}\nRequires={profile['daemonServiceUnit']}\n"
            "[Service]\nType=oneshot\n"
            f"EnvironmentFile={systemd_arg(profile['envFile'], escape_environment=False)}\n"
            f"ExecStart={systemd_arg('/usr/bin/python3')} {systemd_arg(home / '.local/share/onedrive-davfs/tools/profile-sync.py')} {systemd_arg(url)}\n")
        (temporary / sync_unit).write_text(sync)
        (temporary / f"onedrive-davfs-{library_id}-sync.timer").write_text(
            f"[Unit]\nDescription=OneDrive library metadata timer\n[Timer]\nOnBootSec=30s\nOnUnitActiveSec=30s\nUnit={sync_unit}\n"
            "[Install]\nWantedBy=timers.target\n")
        (temporary / "davfs-secrets.entry").write_text(f"{url} daemon {secret}\n")
        (temporary / "davfs-secrets.entry").chmod(0o600)
        escaped_mount = str(mount_point).replace("\\", "\\134").replace(" ", "\\040").replace("\t", "\\011")
        (temporary / "fstab.entry").write_text(f"{url} {escaped_mount} davfs noauto,user,rw,noexec,nosuid,nodev 0 0\n")
        os.rename(temporary, final)
        registry["libraries"].append(profile)
        publish_registry(registry_path, registry)
    except BaseException:
        if profile in registry["libraries"]:
            registry["libraries"].remove(profile)
        if temporary.exists():
            shutil.rmtree(temporary)
        elif final.exists():
            shutil.rmtree(final)
        raise
    return profile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", type=Path, default=Path.home() / ".config/onedrive-davfs/profiles.json")
    commands = parser.add_subparsers(dest="command", required=True)
    account_parser = commands.add_parser("account")
    account_parser.add_argument("--id", required=True)
    account_parser.add_argument("--client-id", required=True)
    account_parser.add_argument("--tenant", default="common")
    account_parser.add_argument("--scopes", default=DEFAULT_SCOPES)
    library_parser = commands.add_parser("library")
    library_parser.add_argument("--source")
    library_parser.add_argument("--account")
    library_parser.add_argument("--source-url")
    library_parser.add_argument("--source-env")
    library_parser.add_argument("--selector", required=True)
    library_parser.add_argument("--name", required=True)
    library_parser.add_argument("--id")
    library_parser.add_argument("--mount-point")
    remove_parser = commands.add_parser("remove-library")
    remove_parser.add_argument("--id", required=True)
    args = parser.parse_args()
    registry_path = args.registry.expanduser().absolute()
    with credential_lock(registry_path.parent):
        registry = read_registry(registry_path)
        if args.command == "remove-library":
            removed = next((library for library in registry["libraries"] if library["id"] == args.id), None)
            if not removed:
                raise ValueError("Library profile not found")
            registry["libraries"] = [library for library in registry["libraries"] if library["id"] != args.id]
            publish_registry(registry_path, registry)
            print(json.dumps({"ok": True, "removed": args.id, "retainedStateDir": removed["stateDir"],
                              "message": "Registry entry removed; stop/disable this profile's units separately. Cached data and account credentials were preserved."}))
            return
        if args.command == "account":
            account_id = identifier(args.id)
            if any(account["id"] == account_id for account in registry["accounts"]):
                raise ValueError("Account profile already exists")
            tenant = authority(args.tenant)
            auth_scopes = scopes(args.scopes)
            if not re.fullmatch(r"[A-Za-z0-9.-]{1,128}", args.client_id):
                raise ValueError("Invalid public-client application ID")
            account_dir = registry_path.parent / "account-data" / account_id
            account_dir.mkdir(parents=True, mode=0o700)
            account = {"id": account_id, "clientId": args.client_id,
                       "tenantId": tenant, "scopes": auth_scopes,
                       "credentialDir": str(account_dir)}
            registry["accounts"].append(account)
            publish_registry(registry_path, registry)
            print(json.dumps({"ok": True, "account": account}))
            return
        if args.source:
            source = next((library for library in registry["libraries"] if library["id"] == args.source), None)
            if not source:
                raise ValueError("Discovery source profile is missing")
            account_id = source["accountId"]
        else:
            if not args.account or not args.source_url or not args.source_env:
                raise ValueError("First library requires --account, --source-url, and --source-env")
            account_id = args.account
            source = {"url": args.source_url, "envFile": args.source_env}
        account = next((account for account in registry["accounts"] if account["id"] == account_id), None)
        if not account:
            raise ValueError("Unknown account profile")
        drive, values = resolve_target(source, args.selector)
        with credential_lock(account["credentialDir"]):
            account_token = json.loads((Path(account["credentialDir"]) / "token.json").read_text())
            if not account_token.get("credential_id") or account_token["credential_id"] != drive.get("credentialId"):
                raise ValueError("Discovery source is not using this account's current login session; start a bootstrap host with --account-dir")
        if (values.get("ONEDRIVE_CLIENT_ID") != account["clientId"]
                or values.get("ONEDRIVE_TENANT_ID", "common") != account["tenantId"]
                or drive["tenantId"] != account["tenantId"]
                or (account.get("graphAccountId") and account["graphAccountId"] != drive["accountId"])):
            raise ValueError("Discovery source belongs to a different account/tenant configuration")
        if set(scopes(values.get("ONEDRIVE_SCOPES", DEFAULT_SCOPES)).split()) != set(account["scopes"].split()):
            raise ValueError("Discovery source and account must use the same delegated scopes")
        account["graphAccountId"] = drive["accountId"]
        library_id = identifier(args.id or "library-" + secrets.token_hex(6))
        profile = create_library(
            registry_path, registry, account, drive, args.name, library_id,
            args.mount_point or Path.home() / "OneDrive-Libraries" / library_id,
            available_port(registry["libraries"]))
        print(json.dumps({"ok": True, "profile": profile}))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError) as error:
        print(json.dumps({"ok": False, "error": str(error)}))
        raise SystemExit(1)
