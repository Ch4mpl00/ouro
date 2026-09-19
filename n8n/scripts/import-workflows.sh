#!/usr/bin/env bash
# Validate the exports, then import (or refresh) them in the running n8n
# container. Import matches on the `id` field, so re-running this after an edit
# updates the same workflows instead of creating copies.
#
#   n8n/scripts/import-workflows.sh
#
# Credentials are NOT imported — provider keys live only in n8n's own encrypted
# store (see README, "First run").
set -euo pipefail

cd "$(dirname "$0")/../.."

COMPOSE=(docker compose -f docker-compose.yml -f n8n/docker-compose.n8n.yml)

node n8n/scripts/validate-workflows.mjs

"${COMPOSE[@]}" exec -T n8n n8n import:workflow --separate --input=/workflows

cat <<'NOTE'

Imported. Still manual, once:
  1. open the editor (ssh -L 5678:localhost:5678 root@<droplet>, then http://localhost:5678)
  2. create the two model credentials and re-select them on the model nodes
  3. set Config → telegramChatId
  4. activate:
       docker compose -f docker-compose.yml -f n8n/docker-compose.n8n.yml \
         exec -T n8n n8n update:workflow --id=news-digest-daily --active=true
NOTE
