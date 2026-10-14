# onedrive-davfs

A WebDAV server for OneDrive, compiled to a `wasi:http/proxy` WebAssembly
component (`wasm32-wasip2`) and served by a dedicated native Rust host built
on Wasmtime. It exists as a capability-sandboxed alternative to `rclone mount`
for syncing OneDrive on Linux, and is the intended eventual backend for the
[`onedrive-sync`](https://github.com/uhansen/onedrive-sync) Omarchy plugin
(that integration is a separate, later piece of work -- see "Non-goals"
below).

In normal use the host runs as a `systemd --user` daemon and is enabled for
startup/login with `systemctl --user enable onedrive-davfs.service`.

For a full install walkthrough, see [INSTALL.md](INSTALL.md).
For notes on why this repo stays on WASI Preview 2 today, see
[docs/wasip3-feasibility.md](docs/wasip3-feasibility.md).

Prebuilt release assets include both the `.wasm` component and the native
`onedrive-davfs-host` Linux binary. CI builds/tests the host and guest, and
validates the component on pushes and pull requests.

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
what's explicitly granted by the native host.

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

- A local **metadata index** served from `/state/index.bin`. `PROPFIND`
  and `HEAD` read a segmented snapshot (directory table + per-folder
  child blocks). Graph `/delta` keeps it fresh via an authenticated
  `POST` with `X-OneDrive-Sync: 1`, driven by
  `onedrive-davfs-sync.timer` every 30s. Local `PUT`/`MKCOL`/`DELETE`/`MOVE`
  patch the snapshot immediately. `ONEDRIVE_INDEX_ENABLED=0` restores
  live Graph listings. A miss or corrupt snapshot always falls back to
  Graph, so a bad index cannot break the mount.
- Plugin JSON endpoints, gated on `GET` + `X-OneDrive-Plugin: 1` (davfs2
  never sends this header, so OneDrive folders named `_status`/`_tree`/`_search`
  stay reachable as WebDAV):
  - `GET /_status` -- index crawl progress, item counts, last `/_sync` tick
    and the current bound drive identity (tenant, selector, drive id/name/type)
  - `GET /_tree?path=/…` -- one directory of the index (Graph fallback on miss)
  - `GET /_search?q=<term>` -- case-insensitive substring search over the
    index by name, files **and** folders (unlike `/_tree`, which is
    folders-only). Read-only, served entirely from the local snapshot — no
    Graph call. Returns `{"ok":true,"ready":false}` if the index isn't
    enabled or hasn't crawled yet; `q` shorter than 2 characters returns an
    empty result set; results are capped (100) with `"truncated":true` if
    the cap was hit, directories sorted first, then by path.
  - `GET /_libraries` -- scoped library discovery for the signed-in account.
    With no query it lists the user's accessible drives (`/me/drives`);
    `groupId=<id>` scopes to one Microsoft 365 group; `siteId=<id>` scopes to
    one SharePoint site; `siteUrl=<https://tenant.sharepoint.com/sites/...>`
    resolves that site first, then lists its libraries.

These report **metadata-index** progress (items applied per tick), not
file-transfer bandwidth: each `GET`/`PUT` is still a direct Graph call.

Explicitly **not** implemented yet:

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
   Add `Sites.Read.All` only if you want scoped SharePoint site discovery.
5. Copy the **Application (client) ID** from the Overview page --
   that's `ONEDRIVE_CLIENT_ID` below. No client secret is needed (this is
   a public client using the device code or browser/PKCE flow).

### 2. First-time sign-in (device code flow, run once)

Interactive browser sign-in can't happen inside the wasm sandbox, so it stays
in native helpers. You can use either:

```sh
ONEDRIVE_CLIENT_ID=<your-app-id> ./tools/device-code-login.sh
```

or:

```sh
ONEDRIVE_CLIENT_ID=<your-app-id> python3 ./tools/browser-login.py
```

The device-code helper prints a URL and short code; the browser helper opens
the authorization page directly, listens on a loopback callback, and completes
an authorization-code + PKCE exchange. Both write
`~/.local/state/onedrive-davfs/token.json` containing a refresh token.
Re-run a helper only if that refresh token is revoked; the daemon itself keeps
it alive by refreshing it before each expiry.

### 3. Configure and run the daemon

```sh
mkdir -p ~/.local/share/onedrive-davfs ~/.local/state/onedrive-davfs ~/.local/bin
cp target/wasm32-wasip2/release/onedrive_davfs.wasm ~/.local/share/onedrive-davfs/
install -m 755 host/target/release/onedrive-davfs-host ~/.local/bin/onedrive-davfs-host
cp systemd/onedrive-davfs.service ~/.config/systemd/user/
install -m 700 -d ~/.config/onedrive-davfs
install -m 600 systemd/onedrive-davfs.env.example ~/.config/onedrive-davfs/env
$EDITOR ~/.config/onedrive-davfs/env   # set ONEDRIVE_BASIC_AUTH_SECRET (openssl rand -base64 32), and ONEDRIVE_CLIENT_ID if the daemon must refresh
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs.service
cp systemd/onedrive-davfs-sync.service systemd/onedrive-davfs-sync.timer \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now onedrive-davfs-sync.timer
```

> `onedrive-davfs-host` does not inherit its whole environment into the guest.
> It forwards only the documented OneDrive variables and preopens only the
> configured state directory as `/state`.

If you already have a valid `token.json` with a non-expired `access_token`,
the daemon can start **without** `ONEDRIVE_CLIENT_ID` and use that token
directly. `ONEDRIVE_CLIENT_ID` becomes mandatory only when the daemon needs
to refresh an expired token.

`ONEDRIVE_DRIVE_BASE` defaults to `me/drive`, which targets the signed-in
account's primary drive. You can also set it to:

- a plain drive ID (`B087983F641B9ED3`)
- `drives/<drive-id>`
- `users/<user-id>/drive`
- `groups/<group-id>/drive`
- `sites/<site-id>/drive`

The daemon canonicalizes that selector and binds the persisted local index to
the resolved drive identity. Switching the selector or signed-in account in an
existing state directory forces a fresh crawl instead of reusing stale delta
links or search metadata from the previous drive.

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
cargo build --target wasm32-wasip2 --release
cargo build --manifest-path host/Cargo.toml --release
wasm-tools validate target/wasm32-wasip2/release/onedrive_davfs.wasm
```

## Testing

```sh
cargo test --lib     # unit tests: date formatting, XML escaping, multistatus shape
cargo test --manifest-path host/Cargo.toml
```

Manual smoke test once a token is in place (the daemon refuses to start
without a real shared secret, so pass one here too):

```sh
SECRET=$(openssl rand -base64 32)
ONEDRIVE_TENANT_ID=common \
ONEDRIVE_CLIENT_ID=<id> \
ONEDRIVE_BASIC_AUTH_SECRET="$SECRET" \
host/target/release/onedrive-davfs-host \
  --addr 127.0.0.1:8765 \
  --state-dir ~/.local/state/onedrive-davfs \
  --component target/wasm32-wasip2/release/onedrive_davfs.wasm &
curl -u "daemon:$SECRET" -X PROPFIND -H 'Depth: 0' http://127.0.0.1:8765/
```

## Security model

- **Loopback is not a trust boundary.** Every request must carry
  `Authorization: Basic` with the password equal to
  `ONEDRIVE_BASIC_AUTH_SECRET`. The daemon **fails closed**: it returns
  `500` with a config error for every request if the secret is unset,
  shorter than 16 characters, or a known placeholder (`REPLACE_ME`, ...).
  Only `ONEDRIVE_ALLOW_UNAUTHENTICATED=1` disables this, for local
  development; never use it on a multi-user machine. The comparison is
  constant-time.
- **Secrets never live in the unit file.** `onedrive-davfs.service` reads
  `~/.config/onedrive-davfs/env` (mode 600) via `EnvironmentFile=`; the
  same secret goes in `~/.davfs2/secrets` (mode 600).
  `tools/device-code-login.sh` runs under `umask 077`, creates the state
  directory `0700` and `token.json` `0600` atomically, posts OAuth form
  bodies to `curl` on stdin (`--data-binary @-`, never `-d` on argv), and
  passes the token response to `python3` over stdin.
- **Paths are sanitised.** Request paths and `MOVE` `Destination` values
  are percent-decoded, `.`/`..` segments and control characters are
  rejected with `400`, and every segment is re-encoded before it is
  interpolated into a Graph `root:/…:` address. Query strings are dropped.
  Absolute `Destination` URLs are accepted only when the host is loopback.
- **Outbound requests only go to Microsoft.** The HTTP client is
  `https://`-only; the native host only allows exact Microsoft OAuth/Graph
  endpoints plus narrow Microsoft OneDrive/SharePoint content domains,
  rejects IP literals and non-public resolved addresses, and connects with
  explicit TLS hostname validation. The bearer token is attached only to
  `graph.microsoft.com` requests (pagination `@odata.nextLink` values are
  checked against that origin and capped at 500 pages); the pre-authenticated
  `/content` download redirect is followed *without* the bearer, exactly
  one hop, and only when `Location` is `https://`. Upload-session URLs
  are treated the same way.
- **Guest capabilities are explicit.** The host forwards only the documented
  OneDrive env vars, preopens only `/state`, keeps guest socket/name-lookup
  APIs disabled, serializes guest invocations with a bounded queue, and
  applies store limits and epoch interruption.
- **No token logging.** The component emits no log lines; Graph error
  bodies are relayed to the client as `502` text but never include the
  `Authorization` header or token endpoint payloads.
- Request bodies are only read for `PUT` (`PROPFIND`/`LOCK` bodies are
  ignored, so there is no XML parsing surface). `PUT` is buffered up to
  64 MiB; files above Graph's 4 MiB simple-upload ceiling use a
  `createUploadSession` with chunked `Content-Range` PUTs.
- **Large downloads still buffer inside the guest.** The current WebDAV `GET`
  path still materializes the full Graph response body in guest memory before
  replying. The host therefore sets an explicit WebAssembly memory ceiling,
  but truly large single-file downloads remain a guest-side limitation until
  the download path is made streaming.

## Repo layout

```
src/
  lib.rs          Guest impl for wasi:http/incoming-handler, request dispatch
  dav.rs          WebDAV verb handlers (PROPFIND/GET/PUT/MKCOL/DELETE/MOVE/LOCK)
  xml.rs          multistatus building, http_date, xml_escape, pct_encode
  graph.rs        Microsoft Graph client (stat/children/delta/get/put/create/delete/move)
  index.rs        id-keyed namespace (nodes + child sets) used during /delta apply
  snapshot.rs     segmented on-disk index.bin (seekable PROPFIND/HEAD)
  sync.rs         POST /_sync: bounded /delta crawl + 410 shadow resync
  auth.rs         OAuth2 refresh-token handling
  config.rs       env var + preopened state-dir configuration
  http_client.rs  generic blocking HTTP client over wasi:http/outgoing-handler
  state_file.rs   read/write a file in the preopened state directory
tools/
  device-code-login.sh   native, one-time OAuth device code bootstrap
  sync-tick.sh           systemd timer helper: POST /_sync without secret-on-argv
systemd/
  onedrive-davfs.service         the daemon (native Wasmtime host + Wasm component)
  onedrive-davfs.env.example     template for ~/.config/onedrive-davfs/env (chmod 600)
  onedrive-davfs-mount.service   optional: davfs2 mount as its own unit
  onedrive-davfs-sync.service    one-shot POST /_sync tick
  onedrive-davfs-sync.timer      every 30s
host/
  src/main.rs       native Wasmtime embedder with explicit env/fs/http policy
wit/
  world.wit       component world (wasi:http/proxy + filesystem/cli imports)
```

## Non-goals for this repo (currently)

- No changes to the `onedrive-sync` Omarchy plugin. That plugin currently
  talks to `rclone mount`; teaching it to talk to this daemon instead (or
  in addition) is a deliberate follow-up, not part of this repo's scope.

## License

MIT — see [LICENSE](LICENSE).
