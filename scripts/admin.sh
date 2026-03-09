#!/usr/bin/env bash
# poly-notifier admin CLI shortcuts
# Usage:
#   ./scripts/admin.sh users                      - list all users
#   ./scripts/admin.sh stats                      - system stats
#   ./scripts/admin.sh tier <telegram_id> <tier>  - set tier (free/premium/unlimited)
#   ./scripts/admin.sh limit <telegram_id> <n>    - set max subscriptions

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/../.env"
if [[ -f "$ENV_FILE" ]]; then
  set -a
  source "$ENV_FILE"
  set +a
fi

HOST="${ADMIN_HOST:-http://localhost:36363}"
PASSWORD="${ADMIN_PASSWORD:?Set ADMIN_PASSWORD env var}"
AUTH="Authorization: Bearer $PASSWORD"

case "${1:-help}" in
  users)
    curl -s -H "$AUTH" "$HOST/admin/users" | jq .
    ;;
  stats)
    curl -s -H "$AUTH" "$HOST/admin/stats" | jq .
    ;;
  tier)
    [[ $# -lt 3 ]] && echo "Usage: $0 tier <telegram_id> <free|premium|unlimited>" && exit 1
    curl -s -X PUT -H "$AUTH" -H "Content-Type: application/json" \
      -d "{\"tier\":\"$3\"}" "$HOST/admin/users/$2/tier" | jq .
    ;;
  limit)
    [[ $# -lt 3 ]] && echo "Usage: $0 limit <telegram_id> <max_subscriptions>" && exit 1
    curl -s -X PUT -H "$AUTH" -H "Content-Type: application/json" \
      -d "{\"max_subscriptions\":$3}" "$HOST/admin/users/$2/limit" | jq .
    ;;
  *)
    echo "poly-notifier admin CLI"
    echo ""
    echo "Usage: $0 <command> [args]"
    echo ""
    echo "Commands:"
    echo "  users                         List all users"
    echo "  stats                         System statistics"
    echo "  tier  <telegram_id> <tier>    Set user tier (free/premium/unlimited)"
    echo "  limit <telegram_id> <n>       Set max subscriptions"
    echo ""
    echo "Env: ADMIN_PASSWORD (required), ADMIN_HOST (default: http://localhost:36363)"
    ;;
esac
