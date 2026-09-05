# Installing `onedrive-davfs`

This guide walks through a full local installation of the `onedrive-davfs`
component, from Azure app registration to a mounted `~/OneDrive` directory.

## 1. Install runtime prerequisites

For the normal install path, you only need:

- `wasmtime`
- `davfs2`
- `curl`, `bash`, and `python3`
- optional but recommended: `gh` for provenance verification

Example on Debian/Ubuntu:

```sh
sudo apt update
sudo apt install -y davfs2 curl python3 gh
```

Install `wasmtime` however you normally manage it, then confirm:

```sh
wasmtime --version
```

## 2. Register an Azure app for OneDrive access

Create a Microsoft Entra ID app registration for the account you want to
mount.

1. Open <https://portal.azure.com>.
2. Go to **Microsoft Entra ID** -> **App registrations** -> **New registration**.
3. Choose **Accounts in any organizational directory and personal Microsoft accounts** unless you intentionally want a narrower scope.
4. Under **Authentication**, add a **Mobile and desktop applications** platform with this redirect URI:

   ```text
   https://login.microsoftonline.com/common/oauth2/nativeclient
   ```

5. Enable **Allow public client flows**.
6. Under **API permissions**, add delegated Microsoft Graph permissions:
   - `Files.ReadWrite.All`
   - `offline_access`
   - `User.Read`
7. Copy the **Application (client) ID**. You will use it as `ONEDRIVE_CLIENT_ID`.

## 3. Download the prebuilt WebAssembly component

Create a place to install the downloaded artifact:

```sh
install -d ~/.local/share/onedrive-davfs
```

Set the release tag you want to install:

```sh
VERSION=v0.1.0
```

Download the component and checksum from GitHub Releases:

```sh
curl -L -o ~/.local/share/onedrive-davfs/onedrive_davfs.wasm \
  "https://github.com/uhansen/onedrive-davfs/releases/download/${VERSION}/onedrive_davfs.wasm"
curl -L -o /tmp/onedrive_davfs.wasm.sha256 \
  "https://github.com/uhansen/onedrive-davfs/releases/download/${VERSION}/onedrive_davfs.wasm.sha256"
```

Verify the checksum:

```sh
(cd ~/.local/share/onedrive-davfs && sha256sum -c /tmp/onedrive_davfs.wasm.sha256)
```

Optional: verify GitHub build provenance:

```sh
gh attestation verify ~/.local/share/onedrive-davfs/onedrive_davfs.wasm \
  --repo uhansen/onedrive-davfs
```

## 4. Perform the one-time OAuth device login

This creates `~/.local/state/onedrive-davfs/token.json`, which the daemon uses
and refreshes later.

```sh
ONEDRIVE_CLIENT_ID=<your-app-id> ./tools/device-code-login.sh
```

The script will print:

- a Microsoft login URL
- a short device code

Complete the sign-in in a browser and wait for the script to finish.

## 5. Install the systemd files

```sh
install -d ~/.local/state/onedrive-davfs
install -d ~/.config/systemd/user
install -m 644 systemd/onedrive-davfs.service ~/.config/systemd/user/
install -m 644 systemd/onedrive-davfs-mount.service ~/.config/systemd/user/
```

## 6. Create the daemon environment file

The service reads configuration from `~/.config/onedrive-davfs/env`.

```sh
install -m 700 -d ~/.config/onedrive-davfs
install -m 600 systemd/onedrive-davfs.env.example ~/.config/onedrive-davfs/env
```

Edit the file:

```sh
$EDITOR ~/.config/onedrive-davfs/env
```

Set at least:

```dotenv
ONEDRIVE_CLIENT_ID=<your-app-id>
ONEDRIVE_TENANT_ID=common
ONEDRIVE_DRIVE_BASE=me/drive
ONEDRIVE_BASIC_AUTH_SECRET=<long-random-secret>
```

Generate the shared secret with:

```sh
openssl rand -base64 32
```

Notes:

- `ONEDRIVE_DRIVE_BASE=me/drive` is the default for the signed-in account's primary drive.
- You can also use a plain drive ID such as `B087983F641B9ED3`.
- Keep this file mode `600`.

## 7. Enable the daemon

Reload the user units and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs.service
```

Check status:

```sh
systemctl --user status --no-pager onedrive-davfs.service
```

The service runs `wasmtime serve` with `-S cli=y`, exports the configured env
vars into the guest, and binds the WebDAV endpoint to `127.0.0.1:8765`.

## 8. Configure `davfs2` credentials

Create the local mount directory and `davfs2` secret entry:

```sh
mkdir -p ~/OneDrive ~/.davfs2
printf 'http://127.0.0.1:8765/  daemon  %s\n' "<same-secret-as-ONEDRIVE_BASIC_AUTH_SECRET>" >> ~/.davfs2/secrets
chmod 600 ~/.davfs2/secrets
```

The password in `~/.davfs2/secrets` must exactly match
`ONEDRIVE_BASIC_AUTH_SECRET`.

## 9. Mount OneDrive

You have two supported options.

### Option A: `fstab` + manual mount

Add this line to `/etc/fstab`, replacing `/home/you/OneDrive` with your real
home path:

```text
http://127.0.0.1:8765/ /home/you/OneDrive davfs noauto,user,rw 0 0
```

Then mount it:

```sh
mount ~/OneDrive
```

### Option B: user systemd mount unit

Enable the provided user service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs-mount.service
```

That unit calls:

```text
/usr/bin/mount.davfs http://127.0.0.1:8765/ %h/OneDrive -o rw,noexec,uid=%U
```

## 10. Smoke-test the mount

First verify the daemon answers WebDAV requests:

```sh
curl -u "daemon:<your-secret>" -X PROPFIND -H 'Depth: 0' http://127.0.0.1:8765/
```

Then test the mounted directory:

```sh
echo hello > ~/OneDrive/onedrive-davfs-test.txt
cat ~/OneDrive/onedrive-davfs-test.txt
rm ~/OneDrive/onedrive-davfs-test.txt
```

## 11. Troubleshooting

### Daemon does not start

Check the user service logs:

```sh
journalctl --user -u onedrive-davfs.service --no-pager -n 100
```

Common causes:

- `ONEDRIVE_BASIC_AUTH_SECRET` is missing or still set to a placeholder
- `ONEDRIVE_CLIENT_ID` is missing and the access token must be refreshed
- the state directory was not exposed to the guest as `/state`

### Mount asks for credentials or fails authentication

Make sure:

- `~/.davfs2/secrets` is mode `600`
- the username is `daemon`
- the password matches `ONEDRIVE_BASIC_AUTH_SECRET` exactly

### Wrong drive is mounted

Set `ONEDRIVE_DRIVE_BASE` explicitly in `~/.config/onedrive-davfs/env`, then restart the daemon:

```sh
systemctl --user restart onedrive-davfs.service
```

Use:

- `me/drive` for the signed-in account's primary drive
- a specific drive ID for an exact target

## 12. Alternative: build the component from source

If you do not want to use a prebuilt release artifact, build it locally
instead.

Install the extra build prerequisites:

- Rust with the `wasm32-wasip2` target
- `wasm-tools`

Example:

```sh
rustup target add wasm32-wasip2
cargo install wasm-tools
```

Build and validate:

```sh
cargo build --target wasm32-wasip2 --release
wasm-tools validate target/wasm32-wasip2/release/onedrive_davfs.wasm
```

Then copy the built component into place:

```sh
install -m 644 target/wasm32-wasip2/release/onedrive_davfs.wasm \
  ~/.local/share/onedrive-davfs/
```
