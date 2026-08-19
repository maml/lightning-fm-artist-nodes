# Lightning FM :: Artist Nodes

A headless artist node daemon for [Lightning FM](https://lightning.fm): a Rust binary that runs an embedded Lightning node ([ldk-node](https://github.com/lightningdevkit/ldk-node)) plus an HTTP API, so an artist can receive payments and sell downloads from their own hardware with no platform in the money path.

This is the self-hosted counterpart to the hosted checkout on [lightning.fm](https://lightning.fm). Same buyer-facing protocol, different operator: you.

## What it does

- **Receives payments**: streaming keysend payments and BOLT 11 invoices settle directly to the node's own wallet.
- **Sells downloads**: an L402-style purchase gate mints an invoice per purchase, confirms settlement, and releases the artifact (FLAC, WAV, stems zip, and more). Repeat downloads work via a claim token or the payment preimage.
- **Serves a Lightning address**: LNURL-pay endpoints so `artist@your-domain` resolves straight to the daemon.
- **Registers products over NIP-98**: the desktop app uploads purchasable artifacts and registers listings with a signed HTTP request; the daemon verifies the artist's Nostr key.
- **Admin API, separately keyed**: on-chain sends and LSPS1 channel purchases require an operator key (`ADMIN_PUBKEY`) that is distinct from the artist's publishing key. A leaked music-signing key must never be able to spend funds; the daemon warns loudly if the two are not separated.

## Running

One container per artist via Docker Compose:

```sh
cp .env.example .env    # one mnemonic per artist; keep these out of git
docker compose up -d
curl http://localhost:8081/api/health
```

Each artist gets an isolated node, wallet, and product store. Mnemonics come in through the environment; nothing secret is written to the image. `scripts/` contains helpers for channel bootstrapping and LSPS1 onboarding.

## Layout

- `src/main.rs` node lifecycle + HTTP server (axum)
- `src/gate.rs` purchase gate: invoices, settlement, delivery
- `src/lnurl.rs` LNURL-pay / Lightning address
- `src/nip98.rs` Nostr HTTP auth verification
- `src/admin.rs` operator API: on-chain, channels (LSPS1)
- `src/store.rs` product registry + purchase ledger

## About the launch catalog

The example artist names in `.env.example` and the compose file are Lightning FM's original house-produced test artists, used to exercise these rails end to end before real artists onboarded. They have since been retired to a private test network. We say this plainly rather than have you find it in the history; see the [desktop app README](https://github.com/Lightning-FM/lightning-fm-desktop#about-the-launch-catalog) for the fuller note.

## License

[MIT](LICENSE)
