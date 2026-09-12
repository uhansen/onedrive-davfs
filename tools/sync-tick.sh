#!/usr/bin/env bash
# One-shot Graph /delta tick. Reads ONEDRIVE_BASIC_AUTH_SECRET from the
# environment (systemd EnvironmentFile) and feeds it to curl via stdin so
# it never appears on argv.
set -euo pipefail
: "${ONEDRIVE_BASIC_AUTH_SECRET:?ONEDRIVE_BASIC_AUTH_SECRET is not set}"
printf 'url = "http://127.0.0.1:8765/"\nrequest = POST\nheader = "X-OneDrive-Sync: 1"\nuser = "daemon:%s"\n' \
  "$ONEDRIVE_BASIC_AUTH_SECRET" | curl -sS -f -K -
echo
