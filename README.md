# onedrive-davfs

A WebDAV server for OneDrive, compiled to a `wasi:http/proxy` WebAssembly
component (`wasm32-wasip2`) and run under `wasmtime serve`. It exists as a
capability-sandboxed alternative to `rclone mount` for syncing OneDrive on
Linux, and is the intended eventual backend for the
[`onedrive-sync`](https://github.com/uhansen/onedrive-sync) Omarchy plugin
(that integration is a separate, later piece of work -- see "Non-goals"
below).

## Why WebDAV instead of FUSE?

WASI Preview 2 has no raw syscalls: no `/dev/fuse`, no `mount()`. A pure
Wasm component cannot be a FUSE driver. Instead, this component speaks
plain HTTP/WebDAV, and the kernel-side mount is done by the ordinary
**davfs2** client, exactly as if it were talking to any other WebDAV
server. The component itself only ever:

- **exports** `wasi:http/incoming-handler` -- answers WebDAV requests from
  davfs2 (`PROPFIND`, `GET`, `PUT`, `MKCOL`, `DELETE`, `MOVE`, `LOCK`),
- **imports** `wasi:http/outgoing-handler` -- makes calls to Microsoft
  Graph, with TLS handled entirely host-side (Rust's `rustls`/`ring`
  don't link into `wasm32-wasip2`),
- **imports** `wasi:filesystem/{preopens,types}` -- to read/write a single
  `token.json` inside one preopened state directory,
- **imports** `wasi:cli/environment` -- for configuration.

No OAuth tokens or raw network I/O ever cross a trust boundary that isn't
`wasi:http` itself; there is no ambient filesystem or socket access beyond
what's explicitly granted on the `wasmtime serve` command line.

## Scope of this first pass

Implemented, against the real Microsoft Graph API:

- `PROPFIND` (Depth 0/1; `Depth: infinity` is refused with `403` +
  `propfind-finite-depth` per RFC 4918)
- `GET`, `PUT` (direct/simple upload only, up to Graph's ~4 MiB ceiling)
- `MKCOL`, `DELETE`, `MOVE`
- `LOCK`/`UNLOCK` (fixed no-op success responses -- enough to satisfy
  davfs2's locking requirement for a single-writer mount; Graph has no
  native locking concept worth modeling here)
- Real OAuth2 **refresh**-token handling (see below)
- A defense-in-depth HTTP Basic auth check (shared secret), since the
  daemon only binds to loopback and that alone isn't treated as a
  sufficient trust boundary

Explicitly **not** implemented yet:

- Chunked/resumable upload sessions for files above the simple-upload
  ceiling (`PUT` above ~4 MiB returns `507` instead of silently failing)
- Graph `/delta` change-feed polling, conflict resolution
- A `/status` JSON endpoint or any integration with `onedrive-sync`
- The interactive/first-consent OAuth flow (see below -- this is
  intentionally a separate native script, not part of the sandboxed
  component)

## OAuth setup

### 1. Register an Azure AD app (one-time, per Microsoft account/tenant)

1. Go to <https://portal.azure.com> → Microsoft Entra ID → App
   registrations → New registration.
2. Choose "Accounts in any organizational directory and personal Microsoft
   accounts" (or narrower, if you know you only need one tenant).
3. Under Authentication, add a **Mobile and desktop applications**
   platform with the redirect URI
   `https://login.microsoftonline.com/common/oauth2/nativeclient`, and
   enable "Allow public client flows" (required for the device code
   flow used below).
4. Under API permissions, add delegated Microsoft Graph scopes:
   `Files.ReadWrite.All`, `offline_access`, `User.Read`.
5. Copy the **Application (client) ID** from the Overview page --
   that's `ONEDRIVE_CLIENT_ID` below. No client secret is needed (this is
   a public client using the device code flow).

### 2. First-time sign-in (device code flow, run once)

Interactive browser sign-in can't happen inside the wasm sandbox, so it's
a plain native script:

```sh
ONEDRIVE_CLIENT_ID=<your-app-id> ./tools/device-code-login.sh
```

This prints a URL and a short code, waits for you to complete sign-in in
a browser, and writes `~/.local/state/onedrive-davfs/token.json`
containing a refresh token. Re-run this script only if that refresh token
is ever revoked; the daemon itself keeps it alive by refreshing it before
each expiry.

### 3. Configure and run the daemon

```sh
mkdir -p ~/.local/share/onedrive-davfs ~/.local/state/onedrive-davfs
cp target/wasm32-wasip1/release/onedrive_davfs.wasm ~/.local/share/onedrive-davfs/
cp systemd/onedrive-davfs.service ~/.config/systemd/user/
$EDITOR ~/.config/systemd/user/onedrive-davfs.service   # fill in ONEDRIVE_BASIC_AUTH_SECRET, and ONEDRIVE_CLIENT_ID if the daemon must refresh
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs.service
```

If you already have a valid `token.json` with a non-expired `access_token`,
the daemon can start **without** `ONEDRIVE_CLIENT_ID` and use that token
directly. `ONEDRIVE_CLIENT_ID` becomes mandatory only when the daemon needs
to refresh an expired token.

### 4. Mount with davfs2

```sh
sudo apt install davfs2   # or your distro's equivalent
mkdir -p ~/OneDrive
echo "http://127.0.0.1:8765/  daemon  <same value as ONEDRIVE_BASIC_AUTH_SECRET>" \
  >> ~/.davfs2/secrets
chmod 600 ~/.davfs2/secrets
```

Either add an `fstab` entry (the usual way to drive davfs2 as a non-root
user):

```
http://127.0.0.1:8765/ /home/you/OneDrive davfs noauto,user,rw 0 0
```

and run `mount ~/OneDrive`, or use the companion systemd unit instead:

```sh
cp systemd/onedrive-davfs-mount.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs-mount.service
```

## Building

```sh
rustup target add wasm32-wasip2   # if not already installed
cargo install cargo-component     # if not already installed
cargo component build --release
wasm-tools validate target/wasm32-wasip1/release/onedrive_davfs.wasm
```

## Testing

```sh
cargo test --lib     # unit tests: date formatting, XML escaping, multistatus shape
```

Manual smoke test once a token is in place:

```sh
wasmtime serve --addr 127.0.0.1:8765 \
  --dir ~/.local/state/onedrive-davfs::/state \
  --env ONEDRIVE_TENANT_ID=common \
  --env ONEDRIVE_CLIENT_ID=<id> \
  target/wasm32-wasip1/release/onedrive_davfs.wasm &
curl -X PROPFIND -H 'Depth: 0' http://127.0.0.1:8765/
```

## Repo layout

```
src/
  lib.rs          Guest impl for wasi:http/incoming-handler, request dispatch
  dav.rs          WebDAV verb handlers (PROPFIND/GET/PUT/MKCOL/DELETE/MOVE/LOCK)
  xml.rs          multistatus building, http_date, xml_escape, pct_encode
  graph.rs        Microsoft Graph client (stat/children/get/put/create/delete/move)
  auth.rs         OAuth2 refresh-token handling
  config.rs       env var + preopened state-dir configuration
  http_client.rs  generic blocking HTTP client over wasi:http/outgoing-handler
  state_file.rs   read/write a file in the preopened state directory
tools/
  device-code-login.sh   native, one-time OAuth device code bootstrap
systemd/
  onedrive-davfs.service         the daemon (wasmtime serve)
  onedrive-davfs-mount.service   optional: davfs2 mount as its own unit
wit/
  world.wit       component world (wasi:http/proxy + filesystem/cli imports)
```

## Non-goals for this repo (currently)

- No changes to the `onedrive-sync` Omarchy plugin. That plugin currently
  talks to `rclone mount`; teaching it to talk to this daemon instead (or
  in addition) is a deliberate follow-up, not part of this repo's scope.
- No GitHub Actions / CI in this pass.

## License

Not yet decided for this repo; ask before assuming any particular license
applies.
