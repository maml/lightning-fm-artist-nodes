# Lightning FM :: Artist Nodes

A headless artist node daemon for [Lightning FM](https://lightning.fm): a single Rust binary that runs an embedded Lightning node ([ldk-node](https://github.com/lightningdevkit/ldk-node)) plus an HTTP API, so an artist can receive payments and sell downloads from hardware they control.

This is the exit-rights half of Lightning FM made literal. Your catalog is signed by your key. Sales and zaps settle to your wallet, on your box, with no platform in the money path. If you leave Lightning FM tomorrow, this daemon keeps selling your music and nothing breaks. Leaving costs you nothing.

It is the self-hosted counterpart to the hosted checkout on [lightning.fm](https://lightning.fm). Same buyer-facing protocol, different operator: you.

## Who is this for

| You are | Start here |
|---|---|
| An artist self-hosting on a VPS | [Quick start](#quick-start), then [docs/onboarding.md](docs/onboarding.md). You have a public IP, so you may not need a tunnel; you still need TLS in front of the daemon. |
| An artist on a Raspberry Pi at home | [Quick start](#quick-start), then [docs/tunnel.md](docs/tunnel.md) (your Pi has no public IP; the tunnel also gives you the HTTPS that wallets require), then [docs/onboarding.md](docs/onboarding.md). |
| A developer hacking on the daemon | [Developing](#developing) and the [Layout](#layout) section. The dev compose (`docker-compose.yml`) runs the signet fleet. |

The VPS and the Pi run identical software with identical config. The differences are operational: the Pi usually needs a Cloudflare tunnel to be reachable, and building on the Pi itself takes about 42 minutes, which is why prebuilt aarch64 binaries exist ([Releases](https://github.com/Lightning-FM/lightning-fm-artist-nodes/releases)). Everything else in these docs applies to both.

## What works today

Verified against the code in this repo, not against ambition.

| Feature | Status |
|---|---|
| Receive Lightning payments | Works. BOLT 11 invoices and keysend both settle to the node's own wallet; keysend TLV metadata (track id, sender, timestamp) is logged. |
| Sell downloads (purchase gate) | Works. Fixed price or name-your-price with a floor. Buyer pays an invoice, then downloads with the payment preimage or a claim token; purchases survive restarts. Formats: flac, wav, aiff, alac, mp3, ogg, m4a, aac, opus, zip (stems). Uploads up to 2 GB. |
| Lightning address (LNURL-pay) | Works. `you@your-domain` resolves to this daemon; invoices commit to the metadata hash as LUD-06 requires. Wallets require HTTPS, so you need a tunnel or reverse proxy in front. |
| Admin API | Works. Funding address, balance, peer connect, on-chain send, LSPS1 orders. Every call is NIP-98 signed against `ADMIN_PUBKEY`, a separate operator key; if you leave it unset it falls back to `ARTIST_PUBKEY` and the daemon warns loudly. Set a distinct key. |
| LSPS1 inbound channel purchase | Works. bLIP-51 REST flow, exercised against Megalith. `scripts/lsps1-onboard.sh` drives it end to end. See [docs/onboarding.md](docs/onboarding.md) for the payment catch on a fresh node. |
| LSPS2 just-in-time channels | Rough edge. The built-in defaults point at a signet LSP. On mainnet, set `LSP_NODE_ID` and `LSP_ADDRESS` to a real provider or leave them; when a JIT invoice fails the gate falls back to a plain invoice, which only pays if you already have inbound capacity. |
| Mainnet defaults | Rough edge. `NETWORK` defaults to signet and `ESPLORA_URL` defaults to Mutinynet. Production must set both. The packaged env file does. |
| Chain-sync readiness | Rough edge. Startup waits a fixed 5 seconds after the node starts rather than confirming sync. On a slow chain source the first requests can arrive before the wallet is current. |
| TLS | Not built in. The daemon speaks plain HTTP. Terminate TLS at a Cloudflare tunnel ([docs/tunnel.md](docs/tunnel.md)) or your own reverse proxy. |
| Product catalog listing | Not built. The gate serves one product per slug (`GET /products/{slug}`); there is no index endpoint. A storefront must already know the slugs. |
| Metrics | Not built. `GET /health` returns JSON (status, node id, channel and peer counts). No Prometheus endpoint. |
| Multiple artists per process | Not built, by design. One process, one artist, one wallet. Run several systemd units or containers for several artists. |

## Quick start

On a fresh Debian 12+ or Ubuntu 22.04+ box (VPS or Pi, 64-bit OS required):

```sh
git clone https://github.com/Lightning-FM/lightning-fm-artist-nodes.git
cd lightning-fm-artist-nodes
sudo ./install.sh
```

The installer is idempotent. It:

1. Downloads the prebuilt binary for your architecture (x86_64 or aarch64) from the latest GitHub release, or installs `target/release/lfm-artist-node` if you built one yourself.
2. Creates the `lfm-artist` system user.
3. Scaffolds `/etc/lfm-artist-node.env` from [deploy/lfm-artist-node.env.example](deploy/lfm-artist-node.env.example). It never overwrites an existing env file.
4. Installs and enables the systemd unit ([deploy/lfm-artist-node.service](deploy/lfm-artist-node.service)).

Then:

```sh
sudo nano /etc/lfm-artist-node.env     # fill in the 12 vars, see below
sudo systemctl start lfm-artist-node
curl http://localhost:8090/health
journalctl -u lfm-artist-node -f
```

From here, [docs/onboarding.md](docs/onboarding.md) walks the whole path to a live mainnet storefront: seed generation (offline, by you, never by us), on-chain funding, buying inbound liquidity, and what renewal costs.

## Configuration :: the 12 env vars

`/etc/lfm-artist-node.env` holds exactly these. The file must stay `root:root` mode 600; it contains your seed.

| Var | Required | What it is |
|---|---|---|
| `ARTIST_NAME` | yes | Display name used in invoice descriptions and logs. |
| `NETWORK` | yes | `bitcoin` for production. Also accepts `signet` (the built-in default), `testnet`, `regtest`. Set it explicitly; the default is not mainnet. |
| `LDK_MNEMONIC` | yes | BIP39 seed for the node wallet. Generate it yourself, offline. See [docs/onboarding.md](docs/onboarding.md). |
| `HEALTH_PORT` | yes | HTTP port for the whole API. Packaged default 8090. See [Ports](#ports) before changing it. |
| `PUBLIC_URL` | yes | External base URL of this daemon, e.g. `https://node.example.com`. NIP-98 verification and LNURL callbacks are checked against it, so it must be exactly how the outside world reaches you. |
| `ESPLORA_URL` | one of these two | Esplora API for chain data, e.g. `https://blockstream.info/api`. Zero extra infrastructure, but you are trusting a public block explorer with your node's chain queries. |
| `BITCOIND_RPC_URL` | one of these two | Your own bitcoind as `http://user:pass@host:8332`. When set it wins over Esplora. More setup, no third party in your chain view. |
| `ARTIST_PUBKEY` | yes | Hex Nostr pubkey allowed to upload products (NIP-98 signed, from the desktop app). Uploads are disabled when unset. |
| `ADMIN_PUBKEY` | yes | Hex Nostr pubkey of the operator key for the admin API. Keep it distinct from `ARTIST_PUBKEY`: the key that signs your music must not be able to spend your funds. |
| `LNURL_ADDRESS` | yes | The Lightning address this node answers for, e.g. `you@node.example.com`. Defaults to a slug of `ARTIST_NAME` at the `PUBLIC_URL` host; set it explicitly in production. |
| `LSPS1_API_URL` | yes | LSPS1 provider REST base for buying inbound channels, e.g. `https://megalithic.me/api/lsps1/v1`. |
| `LSPS1_NODE_URI` | yes | The provider's node as `pubkey@host:port`. Connected at startup and trusted for 0-conf channel opens. |

Advanced overrides, not in the packaged env file: `DATA_DIR` (the systemd unit pins it to `/var/lib/lfm-artist-node`), `RGS_URL` (rapid gossip sync source), and the LSPS2 trio `LSP_NODE_ID`, `LSP_ADDRESS`, `LSP_TOKEN` (just-in-time channel provider; the built-in defaults are signet-only). All are read from the environment like the rest; add them to the env file if you need them.

## Ports

The packaged default is **8090**. Before starting, check nothing already listens there:

```sh
ss -tlnp | grep -E ':(8090|8080)\b' || echo "free"
```

The daemon's own built-in fallback is 8080, and 8080 is a bad choice on most node boxes: lnd's REST listener defaults to 8080 (sample-lnd.conf, checked 2026-08-19), so a RaspiBlitz or any machine already running lnd has it taken. The packaged env file sets `HEALTH_PORT=8090` for exactly this reason. If you change it, change the tunnel or proxy config to match.

## Developing

```sh
cargo build            # debug build
cargo test             # unit tests (gate, admin, nip98, store)
cargo run --bin gen_mnemonic   # print a fresh BIP39 mnemonic (dev identities)
cargo run --bin nip98_sign     # sign NIP-98 headers for scripting the API
```

Two compose files:

- `docker-compose.yml` is the **dev fleet**: five signet nodes matching the retired launch catalog, useful for exercising multi-node behavior locally. Not a production shape.
- `docker-compose.prod.yml` is the **single-artist production example**: one node, mainnet, env-driven. Prefer the systemd install for a Pi or VPS; use this if your box is already compose-managed.

Release builds happen in CI on tag push ([.github/workflows/release.yml](.github/workflows/release.yml)), producing Linux x86_64 and aarch64 binaries. They are built on Ubuntu 22.04 runners so they run on Debian 12+ and Ubuntu 22.04+ (needs `libssl3` and `ca-certificates`, both present on stock installs).

## HTTP API summary

Public, no auth:

- `GET /health`, `GET /node-id`, `GET /invoice?amount_sats=N`
- `GET /products/{slug}`, `POST /products/{slug}/invoice`, `GET /products/{slug}/status?claim=...`, `GET /products/{slug}/download?preimage=...` (or `?claim=...`)
- `GET /.well-known/lnurlp/{name}`

Artist-signed (NIP-98, `ARTIST_PUBKEY`):

- `PUT /products/{slug}?title=...&price_sats=...&format=...` with the artifact as the body

Operator-signed (NIP-98, `ADMIN_PUBKEY`):

- `GET /admin/address`, `GET /admin/balance`, `POST /admin/connect`, `POST /admin/send-onchain`, `POST /admin/lsps1-order`, `GET /admin/lsps1-order/{order_id}`

Block `/admin` at your tunnel or proxy anyway; the signature gate is real but there is no reason to expose the surface. [docs/tunnel.md](docs/tunnel.md) shows how.

## Layout

- `src/main.rs` node lifecycle + HTTP server (axum)
- `src/gate.rs` purchase gate: invoices, settlement, delivery
- `src/lnurl.rs` LNURL-pay / Lightning address
- `src/nip98.rs` Nostr HTTP auth verification
- `src/admin.rs` operator API: on-chain, channels (LSPS1)
- `src/store.rs` product registry + purchase ledger
- `scripts/` channel bootstrapping and LSPS1 onboarding helpers
- `deploy/` systemd unit + production env template
- `docs/` onboarding walkthrough, Cloudflare tunnel recipe
- `install.sh` idempotent installer for a fresh VPS or Pi

## About the launch catalog

The example artist names in `.env.example` and the dev compose file are Lightning FM's original house-produced test artists, used to exercise these rails end to end before real artists onboarded. They have since been retired to a private test network. We say this plainly rather than have you find it in the history; see the [desktop app README](https://github.com/Lightning-FM/lightning-fm-desktop#about-the-launch-catalog) for the fuller note.

## License

[MIT](LICENSE)
