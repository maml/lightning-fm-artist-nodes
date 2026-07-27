#!/usr/bin/env bash
#
# lsps1-onboard.sh — buy an artist node its inbound channel via LSPS1.
#
# Drives the daemon's NIP-98-gated admin API end to end: connect to the
# LSPS1 provider, create an order, pay it (from the node's own on-chain
# wallet by default), and wait for the 0-conf channel to appear.
#
# Usage:
#   ARTIST_SECRET=<hex> ./scripts/lsps1-onboard.sh
#
# Env:
#   DAEMON_URL       daemon base (default http://localhost:8080; must match
#                    the daemon's PUBLIC_URL for NIP-98)
#   ARTIST_SECRET    hex Nostr secret matching the daemon's ARTIST_PUBKEY
#   LSP_BALANCE_SAT  inbound capacity to buy (default 500000)
#   LSPS1_NODE_URI   provider node URI — connect step (skipped if unset;
#                    the daemon may have auto-connected at startup)
#   PAY_MODE         onchain (default: node pays from its own wallet)
#                    | manual (print payment details and exit)

set -euo pipefail

DAEMON_URL="${DAEMON_URL:-http://localhost:8080}"
LSP_BALANCE_SAT="${LSP_BALANCE_SAT:-500000}"
PAY_MODE="${PAY_MODE:-onchain}"
: "${ARTIST_SECRET:?ARTIST_SECRET required (hex Nostr secret key)}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIGN="$HERE/target/debug/nip98_sign"
[[ -x "$SIGN" ]] || (cd "$HERE" && cargo build --bin nip98_sign --quiet)

say()  { echo "[lsps1-onboard] $1"; }
fail() { echo "[lsps1-onboard] ✗ $1" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"

# NIP-98-signed request. usage: req METHOD PATH [JSON_BODY]
req() {
  local method=$1 path=$2 body=${3:-}
  local url="$DAEMON_URL$path"
  if [[ -n "$body" ]]; then
    local hash
    hash=$(printf '%s' "$body" | shasum -a 256 | cut -d' ' -f1)
    local auth
    auth=$("$SIGN" header "$ARTIST_SECRET" "$method" "$url" "$hash")
    curl -sS -X "$method" "$url" -H "Authorization: $auth" \
      -H "Content-Type: application/json" --data-binary "$body"
  else
    local auth
    auth=$("$SIGN" header "$ARTIST_SECRET" "$method" "$url")
    curl -sS -X "$method" "$url" -H "Authorization: $auth"
  fi
}

# ── 0. Daemon + balance ────────────────────────────────────────
HEALTH=$(curl -fsS "$DAEMON_URL/health") || fail "daemon not reachable at $DAEMON_URL"
say "daemon: $(jq -r '.artist' <<<"$HEALTH") ($(jq -r '.network' <<<"$HEALTH"), $(jq -r '.channels' <<<"$HEALTH") channels)"

BALANCE=$(req GET /admin/balance)
jq -e '.error' <<<"$BALANCE" >/dev/null 2>&1 && fail "admin auth failed: $BALANCE"
SPENDABLE=$(jq -r '.spendable_onchain_sats' <<<"$BALANCE")
say "on-chain spendable: $SPENDABLE sats · inbound: $(jq -r '.inbound_capacity_sats' <<<"$BALANCE") sats"

# ── 1. Connect to the provider ─────────────────────────────────
if [[ -n "${LSPS1_NODE_URI:-}" ]]; then
  CONNECT=$(req POST /admin/connect "{\"uri\":\"$LSPS1_NODE_URI\"}")
  jq -e '.connected' <<<"$CONNECT" >/dev/null || fail "connect failed: $CONNECT"
  say "connected to LSPS1 provider"
fi

# ── 2. Create the order ────────────────────────────────────────
ORDER=$(req POST /admin/lsps1-order "{\"lsp_balance_sat\":$LSP_BALANCE_SAT}")
jq -e '.error' <<<"$ORDER" >/dev/null 2>&1 && fail "order failed: $ORDER"
ORDER_ID=$(jq -r '.order_id // empty' <<<"$ORDER")
[[ -n "$ORDER_ID" ]] || fail "no order_id in response: $ORDER"

ONCHAIN_ADDR=$(jq -r '.payment.onchain.address // empty' <<<"$ORDER")
ONCHAIN_TOTAL=$(jq -r '.payment.onchain.order_total_sat // empty' <<<"$ORDER")
BOLT11=$(jq -r '.payment.bolt11.invoice // .payment.bolt11.order_total_sat // empty' <<<"$ORDER")
FEE=$(jq -r '.payment.onchain.fee_total_sat // .payment.bolt11.fee_total_sat // "?"' <<<"$ORDER")

say "order $ORDER_ID: ${LSP_BALANCE_SAT} sats inbound · fee $FEE sats"

# ── 3. Pay ─────────────────────────────────────────────────────
case "$PAY_MODE" in
  manual)
    say "manual payment mode — pay one of:"
    [[ -n "$ONCHAIN_ADDR" ]] && say "  on-chain: $ONCHAIN_TOTAL sats → $ONCHAIN_ADDR"
    [[ -n "$BOLT11" ]] && say "  bolt11: $BOLT11"
    say "then poll: GET $DAEMON_URL/admin/lsps1-order/$ORDER_ID"
    exit 0
    ;;
  onchain)
    [[ -n "$ONCHAIN_ADDR" && -n "$ONCHAIN_TOTAL" ]] || fail "order offers no on-chain payment: $ORDER"
    [[ "$SPENDABLE" -ge "$ONCHAIN_TOTAL" ]] || fail "insufficient on-chain balance: $SPENDABLE < $ONCHAIN_TOTAL sats"
    SEND=$(req POST /admin/send-onchain "{\"address\":\"$ONCHAIN_ADDR\",\"amount_sats\":$ONCHAIN_TOTAL}")
    TXID=$(jq -r '.txid // empty' <<<"$SEND")
    [[ -n "$TXID" ]] || fail "payment send failed: $SEND"
    say "paid $ONCHAIN_TOTAL sats on-chain (txid ${TXID:0:16}…)"
    ;;
  *) fail "unknown PAY_MODE: $PAY_MODE" ;;
esac

# ── 4. Wait for the channel ────────────────────────────────────
say "waiting for the provider to open the channel (0-conf: usually fast once the tx is seen)…"
for i in $(seq 1 90); do
  STATE=$(req GET "/admin/lsps1-order/$ORDER_ID" | jq -r '.order_state // "UNKNOWN"')
  CHANNELS=$(curl -fsS "$DAEMON_URL/health" | jq -r '.channels')
  if [[ "$STATE" == "COMPLETED" || "$CHANNELS" -gt $(jq -r '.channels' <<<"$HEALTH") ]]; then
    say "✓ channel open (order state: $STATE, channels: $CHANNELS)"
    req GET /admin/balance | jq '{inbound_capacity_sats, channels}'
    say "DONE — this node can now receive sales up to its inbound capacity."
    exit 0
  fi
  [[ $((i % 6)) == 0 ]] && say "  still waiting… (order: $STATE)"
  sleep 10
done
fail "channel not open after 15 min — check order: GET $DAEMON_URL/admin/lsps1-order/$ORDER_ID"
