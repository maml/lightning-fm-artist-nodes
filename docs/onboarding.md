# Onboarding :: zero to a mainnet storefront

This walks one artist from nothing to a live, self-hosted storefront on mainnet: seed, keys, config, funding, inbound liquidity, first product. It assumes you finished the README quick start (binary installed, unit enabled, port picked) and, if you are behind a home router, [docs/tunnel.md](tunnel.md).

Budget expectations up front: you need on-chain sats to fund the node, an external Lightning wallet for one payment during setup, and a recurring liquidity cost of roughly 13,000 sats per quarter (details in step 5). None of that goes to Lightning FM.

## 1. Generate your seed :: offline, by you, never by us

The node wallet is a BIP39 mnemonic. You generate it. Nobody at Lightning FM ever sees it, and you should refuse any onboarding flow anywhere that offers to generate it for you.

Options, best first:

- A hardware wallet or any offline BIP39 tool you already trust.
- This repo's helper, run on a machine with the network cable out:

```sh
cargo run --bin gen_mnemonic
```

Write the 12 words on paper. Two copies, two places. The seed recovers on-chain funds; it does NOT recover Lightning channel state by itself, so also treat `/var/lib/lfm-artist-node` as precious once channels exist (snapshot it only while the daemon is stopped).

## 2. Two Nostr keys, two jobs

The daemon separates identity from control:

- `ARTIST_PUBKEY` is your publishing key, the same one the desktop app signs your catalog with. It authorizes product uploads and nothing else.
- `ADMIN_PUBKEY` is a fresh operator key. It authorizes money movement: funding addresses, on-chain sends, channel orders.

Keep them different. If your publishing key ever leaks, the thief can impersonate your uploads but cannot spend your funds. The daemon logs a loud warning if the two match.

Generate an operator keypair with any Nostr tool, then get the hex pubkey:

```sh
cargo run --bin nip98_sign pubkey <operator_secret_hex>
```

The secret stays with you (a password manager is fine); only the pubkey goes in the env file.

## 3. Configure and start

Fill in all 12 vars in `/etc/lfm-artist-node.env`. The two that bite people:

- `NETWORK="bitcoin"`. The built-in default is signet. A signet node looks healthy and receives nothing real.
- Chain source: either keep `ESPLORA_URL` pointed at a mainnet Esplora (simple, but a public explorer sees your node's chain queries) or set `BITCOIND_RPC_URL` to your own bitcoind (it wins over Esplora when set). A RaspiBlitz or any existing node box already has bitcoind; use it.

Then:

```sh
sudo systemctl start lfm-artist-node
curl http://localhost:8090/health
```

Expect `"network":"bitcoin"` and `"channels":0`. Also confirm the public path works: `curl https://node.example.com/health`.

## 4. Fund the node on-chain

Admin calls are NIP-98 signed. The signature commits to the exact public URL, so `DAEMON_URL` must equal the daemon's `PUBLIC_URL`. Get a funding address:

```sh
SIGN=./target/debug/nip98_sign   # cargo build --bin nip98_sign
URL=https://node.example.com/admin/address
curl -sS "$URL" -H "Authorization: $($SIGN header <operator_secret_hex> GET $URL)"
```

Send it enough to cover the channel order plus on-chain fees; 50,000 sats is a comfortable start. Watch it arrive:

```sh
URL=https://node.example.com/admin/balance
curl -sS "$URL" -H "Authorization: $($SIGN header <operator_secret_hex> GET $URL)"
```

## 5. Buy inbound liquidity (LSPS1) :: the payment catch

A fresh Lightning node can send nothing and receive nothing. Inbound capacity is bought from an LSP; this daemon speaks the LSPS1 (bLIP-51) REST flow, exercised against Megalith (`LSPS1_API_URL` and `LSPS1_NODE_URI` in the env file).

Here is the catch everyone hits: the LSPS1 order itself must be paid, and **the node cannot pay the order's Lightning invoice, because it has no channels yet**. That is not a bug, it is the bootstrap problem. Two ways through:

- **On-chain, from the node's own wallet** (what step 4 funded). This is the default path of the helper script.
- **Lightning, from an external wallet.** Any wallet with sats can pay the order's BOLT 11 invoice. Use `PAY_MODE=manual` to print the payment details and pay from your phone.

The script drives the whole flow: connect to the provider, create the order, pay it, wait for the 0-conf channel:

```sh
DAEMON_URL=https://node.example.com \
ARTIST_SECRET=<operator_secret_hex> \
LSP_BALANCE_SAT=500000 \
./scripts/lsps1-onboard.sh
```

(`ARTIST_SECRET` is the script's historical name for the operator secret matching `ADMIN_PUBKEY`.)

When it finishes, `/health` shows `"channels":1` and the balance call shows inbound capacity. You can now be paid.

### Renewal reality :: channels expire

LSPS1 channels are leased, not owned. Orders here default to a 13,000-block lease, roughly 90 days. Our most recent Megalith order (2026-08) cost 13,126 sats for a channel expiring in about 91 days. Treat roughly 13,000 sats per quarter as the recurring cost of staying open for business, at the provider's current pricing, which can change.

Put a reminder about 80 days out. Re-run the order before expiry; after it lapses the provider can close the channel and your inbound capacity goes with it. Buyers then see failed payments, not an error page, so you will not notice from the storefront side.

## 6. First product

Products are uploaded from the Lightning FM desktop app, which signs a NIP-98 `PUT /products/{slug}` with your artist key against this node's `PUBLIC_URL`. After an upload, verify from anywhere:

```sh
curl https://node.example.com/products/<slug>
```

Then do one real test purchase end to end from a wallet that is not yours-on-this-node: request the invoice, pay it, download with the preimage. Also test the Lightning address by sending yourself a small zap to `you@node.example.com`.

## 7. The checklist

- `/health` public, `network` is `bitcoin`, `channels` at least 1
- `/admin/*` returns 404 through the tunnel, works on localhost
- env file is root:root mode 600; seed is on paper in two places
- `ADMIN_PUBKEY` differs from `ARTIST_PUBKEY`; no warning in `journalctl -u lfm-artist-node`
- one real purchase completed, one zap received
- channel renewal reminder set (about 80 days out)

That is the whole storefront. Your key signs the catalog, your box holds the files, your wallet takes the money. If you ever want out, take the seed and the data directory and go; nothing here phones home.
