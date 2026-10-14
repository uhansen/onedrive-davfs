#!/usr/bin/env bash
# Native device-code bootstrap. Optional arguments remain state_dir, tenant_id.
set -euo pipefail
exec python3 "$(dirname -- "$0")/device-login.py" "$@"
