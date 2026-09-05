#!/usr/bin/env bash
# One-time (or re-run-if-revoked) OAuth bootstrap for onedrive-davfs.
#
# This is a plain native script, deliberately NOT part of the Wasm
# component: interactive browser-based consent cannot happen inside the
# wasm32-wasip2 sandbox, so the *first* token acquisition happens here,
# out-of-band, using the OAuth 2.0 device authorization grant. The daemon
# itself only ever *refreshes* the token this script writes.
#
# Usage:
#   ONEDRIVE_CLIENT_ID=<app-id> ./tools/device-code-login.sh [state_dir] [tenant_id]
#
# Requires: bash, curl, python3 (for JSON parsing -- avoids a jq dependency).

set -euo pipefail

STATE_DIR="${1:-$HOME/.local/state/onedrive-davfs}"
TENANT_ID="${2:-${ONEDRIVE_TENANT_ID:-common}}"
CLIENT_ID="${ONEDRIVE_CLIENT_ID:?Set ONEDRIVE_CLIENT_ID to your Azure AD app application (client) ID}"
SCOPES="offline_access Files.ReadWrite.All User.Read"

mkdir -p "$STATE_DIR"
TOKEN_FILE="$STATE_DIR/token.json"

echo "Requesting device code from Azure AD (tenant: $TENANT_ID)..." >&2
device_resp=$(curl -sS -X POST \
  "https://login.microsoftonline.com/$TENANT_ID/oauth2/v2.0/devicecode" \
  -d "client_id=$CLIENT_ID" \
  --data-urlencode "scope=$SCOPES")

user_code=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["user_code"])' <<<"$device_resp")
verification_uri=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["verification_uri"])' <<<"$device_resp")
device_code=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["device_code"])' <<<"$device_resp")
interval=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("interval", 5))' <<<"$device_resp")
expires_in=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("expires_in", 900))' <<<"$device_resp")

cat >&2 <<EOF

  Open:  $verification_uri
  Code:  $user_code

Waiting for you to complete sign-in in a browser (times out in ${expires_in}s)...
EOF

deadline=$(( $(date +%s) + expires_in ))
while :; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "Timed out waiting for sign-in." >&2
    exit 1
  fi
  sleep "$interval"
  token_resp=$(curl -sS -X POST \
    "https://login.microsoftonline.com/$TENANT_ID/oauth2/v2.0/token" \
    -d "client_id=$CLIENT_ID" \
    -d "grant_type=urn:ietf:params:oauth:grant-type:device_code" \
    -d "device_code=$device_code")

  error=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("error",""))' <<<"$token_resp")
  case "$error" in
    "") break ;;
    authorization_pending) continue ;;
    slow_down) interval=$((interval + 5)); continue ;;
    *)
      echo "Sign-in failed: $token_resp" >&2
      exit 1
      ;;
  esac
done

python3 - "$token_resp" "$TOKEN_FILE" <<'PY'
import json, sys, time
resp = json.loads(sys.argv[1])
out = {
    "refresh_token": resp["refresh_token"],
    "access_token": resp["access_token"],
    "expires_at": int(time.time()) + int(resp.get("expires_in", 3600)) - 60,
}
with open(sys.argv[2], "w") as f:
    json.dump(out, f, indent=2)
PY
chmod 600 "$TOKEN_FILE"

echo "Wrote $TOKEN_FILE. The onedrive-davfs daemon can now refresh from this token." >&2
