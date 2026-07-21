#!/bin/bash
# Lightning FM — Bootstrap artist node channels via LSPS2
#
# For each artist node:
# 1. Get node_id from the management API
# 2. Request a BOLT 11 invoice (triggers LSPS2 JIT channel from LSP)
# 3. Pay the invoice from the listener node (the desktop app)
#
# Prerequisites:
# - All artist containers running and healthy
# - Listener node (desktop app) running with funded wallet
#
# Usage:
#   ./scripts/bootstrap-channels.sh

set -euo pipefail

AMOUNT_SATS=50000  # Bootstrap amount per artist

# Artist management API ports (mapped in docker-compose.yml)
declare -A ARTISTS=(
  ["Satoshi Sounds"]="8081"
  ["Lightning Louise"]="8082"
  ["Keypair"]="8083"
  ["The Relay Operators"]="8084"
  ["0GGM3NT3D"]="8085"
)

echo "=== Lightning FM — Channel Bootstrap ==="
echo ""

for artist in "${!ARTISTS[@]}"; do
  port="${ARTISTS[$artist]}"
  echo "--- ${artist} (port ${port}) ---"

  # Check health
  health=$(curl -s "http://localhost:${port}/health" 2>/dev/null || echo '{"status":"unreachable"}')
  status=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','unknown'))" 2>/dev/null || echo "error")

  if [ "$status" != "ok" ]; then
    echo "  [SKIP] Node not healthy: ${status}"
    continue
  fi

  # Get node_id
  node_id=$(curl -s "http://localhost:${port}/node-id" | python3 -c "import sys,json; print(json.load(sys.stdin)['node_id'])" 2>/dev/null)
  echo "  Node ID: ${node_id:0:20}..."

  # Check if already has channels
  channels=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('channels',0))" 2>/dev/null || echo "0")
  if [ "$channels" -gt 0 ]; then
    echo "  [SKIP] Already has ${channels} channel(s)"
    continue
  fi

  # Request invoice for channel bootstrap
  invoice_json=$(curl -s "http://localhost:${port}/invoice?amount_sats=${AMOUNT_SATS}" 2>/dev/null)
  bolt11=$(echo "$invoice_json" | python3 -c "import sys,json; print(json.load(sys.stdin)['bolt11'])" 2>/dev/null)

  if [ -z "$bolt11" ]; then
    echo "  [ERROR] Failed to get invoice"
    continue
  fi

  echo "  Invoice: ${bolt11:0:30}..."
  echo "  Amount: ${AMOUNT_SATS} sats"
  echo ""
  echo "  >>> Pay this invoice from the desktop app to bootstrap the channel <<<"
  echo "  >>> Or use: lncli payinvoice ${bolt11:0:40}... <<<"
  echo ""
done

echo ""
echo "After paying all invoices, wait ~30 seconds for channels to confirm on Mutinynet."
echo "Then verify with: curl http://localhost:808X/health (check 'channels' field)"
