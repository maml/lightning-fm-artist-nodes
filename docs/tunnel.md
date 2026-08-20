# Cloudflare Tunnel :: reaching a home Pi (and getting TLS anywhere)

The daemon speaks plain HTTP on `HEALTH_PORT` (default 8090). Two things stand between that and a working storefront:

1. A Pi at home usually has no public IP (CGNAT), so nothing can reach it.
2. Lightning wallets require HTTPS for LNURL-pay, and NIP-98 verification wants a stable public URL.

A Cloudflare tunnel solves both: an outbound-only connection from your box to Cloudflare, TLS terminated at their edge, no ports forwarded on your router. A VPS with a public IP can skip the tunnel and use any reverse proxy that terminates TLS instead; the `/admin` blocking advice below still applies.

You need a domain on Cloudflare (free plan is fine).

## 1. Install cloudflared

```sh
# Pi (aarch64)
curl -fsSL -o cloudflared.deb https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64.deb
# VPS (x86_64)
curl -fsSL -o cloudflared.deb https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64.deb

sudo dpkg -i cloudflared.deb
```

## 2. Authenticate and create the tunnel

```sh
cloudflared tunnel login
cloudflared tunnel create lfm-artist-node
```

`create` prints a tunnel UUID and writes a credentials JSON into `~/.cloudflared/`. Note both.

## 3. Configure ingress, blocking /admin

Write `~/.cloudflared/config.yml`:

```yaml
tunnel: <TUNNEL-UUID>
credentials-file: /home/<you>/.cloudflared/<TUNNEL-UUID>.json

ingress:
  # The admin surface never leaves the box. It is NIP-98 signed anyway,
  # but there is no reason to expose it; run admin calls on the box itself
  # (localhost) or over SSH. Rules match top to bottom, so this comes first.
  - hostname: node.example.com
    path: ^/admin(/.*)?$
    service: http_status:404

  - hostname: node.example.com
    service: http://localhost:8090

  # Required catch-all.
  - service: http_status:404
```

If you changed `HEALTH_PORT`, change the service line to match.

## 4. Route DNS and test

```sh
cloudflared tunnel route dns lfm-artist-node node.example.com
cloudflared tunnel run lfm-artist-node
```

From another network:

```sh
curl https://node.example.com/health          # should return the health JSON
curl -i https://node.example.com/admin/balance  # should return 404 from the tunnel
```

## 5. Install as a service :: the config-copy gotcha

```sh
sudo cloudflared service install
sudo systemctl status cloudflared
```

**Gotcha:** `cloudflared service install` COPIES your config to `/etc/cloudflared/config.yml`. From that moment the service reads only the copy. Later edits to `~/.cloudflared/config.yml` do nothing and will quietly convince you a change "did not work." Edit `/etc/cloudflared/config.yml` instead, then:

```sh
sudo systemctl restart cloudflared
```

## 6. Point the daemon at its public URL

In `/etc/lfm-artist-node.env`:

```sh
PUBLIC_URL="https://node.example.com"
LNURL_ADDRESS="you@node.example.com"
```

Then `sudo systemctl restart lfm-artist-node`. NIP-98 signatures and LNURL callbacks verify against `PUBLIC_URL`, so uploads and zaps break if it does not match the tunnel hostname exactly.

## Upload sizes through the tunnel

The purchase gate accepts artifacts up to 2 GB, but Cloudflare's documentation lists a 100 MB maximum upload size on Free and Pro plans (developers.cloudflare.com/cache/concepts/default-cache-behavior/, checked 2026-08-19). Large WAV or stems uploads from the desktop app will fail through the tunnel. Upload big artifacts over the LAN or an SSH port-forward straight to `localhost:8090` instead; downloads are unaffected because responses stream out, not in.
