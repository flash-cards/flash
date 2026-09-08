# Flash

Spaced-repetition flashcards you study with your AI. Flash is a small
self-hosted server: a web app for your decks and an [MCP](https://modelcontextprotocol.io)
server that lets Claude, ChatGPT, Grok or any MCP client create cards from
what you're learning, quiz you out loud, grade your answers and file the
reviews with the [FSRS](https://github.com/open-spaced-repetition/fsrs-rs)
scheduler. One Rust binary, one SQLite file, no accounts anywhere but yours.

The hosted version, with self-serve signup, Google/Apple sign-in and a
community deck library, is at [flashmemorize.com](https://flashmemorize.com).
iOS and Android apps are on the way and not out yet. This repository is
the core it all runs on.

## Why not Anki + AnkiConnect?

AnkiConnect is a bridge into the desktop app: it only answers while Anki
is open on that machine, and it speaks Anki's own JSON, not MCP. Flash is
a server. It is reachable from your phone, from a Claude connector, from
Claude Code in a terminal, all at once, and the scheduling lives in the
server so every surface sees the same queue. It imports `.apkg` decks
whole (cloze, hints, images, audio, LaTeX, typed answers, nested decks)
and exports them back at any time, so trying it costs nothing.

## Five-minute quickstart (Docker)

```
git clone https://github.com/flash-cards/flash && cd flash
cp .env.example .env            # set FLASH_BASE_URL to the URL you'll reach it at
docker compose up -d
docker compose logs flash | grep enroll
```

The first boot finds an empty database and logs a one-time link:

```
no users yet — enroll the first admin within 24h at:
https://cards.example.com/enroll/<token>
```

Open it, add a passkey or a password, and you are the admin. Every later
account is created from **Settings → Invite**: there is no public signup
on a self-hosted Flash, by design.

Without Docker: `cargo build --release -p flash-server`, then run
`target/release/flash-server` with the same environment variables. The
binary embeds its templates and static assets; it writes under
`FLASH_DATA_DIR`, plus short-lived scratch files for an import or export
in the system temp directory (the systemd unit gives it a private one).

## Connecting your AI

Flash serves MCP at `/mcp` (Streamable HTTP) with OAuth 2.1: the client
registers itself, sends you to Flash's login page, and gets a token
scoped to your account.

- **Claude.ai / Claude mobile** — Settings → Connectors → Add custom
  connector → paste `https://<your host>/mcp`. Claude's connectors need a
  publicly reachable HTTPS origin, so put Flash behind a reverse proxy
  with a certificate, such as Caddy:
  ```
  cards.example.com {
      reverse_proxy 127.0.0.1:8437 {
          header_up X-Real-IP {remote_host}
      }
  }
  ```
  and set `FLASH_CLIENT_IP_HEADER=x-real-ip` so the rate limiters see
  each visitor rather than the proxy (see the note under Configuration).
  A tunnel (Cloudflare Tunnel, Tailscale Funnel, ngrok) works the same
  way if you'd rather not open a port.
- **Claude Code** — `claude mcp add --transport http flash https://<your host>/mcp`.
  Claude Code and Claude Desktop run on your own machine, so a LAN address
  works for the MCP connection as long as `FLASH_BASE_URL` matches what
  you paste. The web UI itself needs HTTPS or `http://localhost`: its
  session cookie is marked Secure, and a browser drops it over plain http
  to any other host (the server says so at boot).
- **ChatGPT** (Plus and up, Developer mode) — Settings → Apps → add the
  same URL as an MCP server. **Grok** — Connectors → New → Custom.

Once connected, say "quiz me on my pharmacology deck" and follow along.
The web app at `/` is where you import decks, edit cards and read your
stats.

## Configuration

Everything is an environment variable; the core needs only the first
three. Optional groups are all-or-nothing: a partial set is a boot error,
an absent set turns the feature off.

| Variable | Default | What it does |
|---|---|---|
| `FLASH_BASE_URL` | — | The public origin (`https://cards.example.com`). Passkeys, OAuth and every link in a mail are minted against it, so it must be what browsers actually see. A hostname, not an IP address: passkeys are bound to a domain, and `http://localhost:8437` is fine for a trial. |
| `FLASH_BIND` | `127.0.0.1:8437` | Listen address. The Docker image sets `0.0.0.0:8437`. |
| `FLASH_CLIENT_IP_HEADER` | unset | Header holding the real client address when a proxy is in front (`x-real-ip`, `cf-connecting-ip`). Unset uses the TCP peer. See the note below the table. |
| `FLASH_DATA_DIR` | `./data` | The SQLite database, import scratch space and media. Back this directory up. |
| `FLASH_SUPPORT_EMAIL` | unset | Shown on the pages that print a contact address. |
| `RESEND_API_KEY` + `FLASH_EMAIL_FROM` | unset | Outbound mail through [Resend](https://resend.com), the one provider supported today. The core sends exactly one kind of mail, the password-reset link, so without this there is simply no self-service reset. Both or neither. |
| `FLASH_DEV_MAIL_LOG=1` | unset | Instead of a provider, log the mail (and its link) to stdout. |
| `FLASH_MEDIA_R2_ENDPOINT`, `FLASH_MEDIA_R2_BUCKET`, `FLASH_MEDIA_R2_ACCESS_KEY_ID`, `FLASH_MEDIA_R2_SECRET_ACCESS_KEY` | unset | Keep media blobs in any S3-compatible bucket (S3, R2, MinIO, B2) instead of under the data directory. Most installs leave this unset. All four or none. |

Accounts are created by an admin (the enroll link above, then
Settings → Invite). Each account signs in with passkeys, a password, or
both, and always keeps at least one method. Studying, reviewing and export
are never gated by anything.

The sign-in, API and web-mutation rate limiters key on the client
address. Behind a reverse proxy every request arrives from the proxy's
address, so without more configuration the whole instance shares one
bucket and the server says so in its log on every boot. Tell Flash which
header carries the real address with `FLASH_CLIENT_IP_HEADER`:
`x-real-ip` for Caddy (with the `header_up` line above) and nginx (with
`proxy_set_header X-Real-IP $remote_addr`), `cf-connecting-ip` for
Cloudflare. `x-forwarded-for` works too: the rightmost address, the one
your proxy appended, is the one used. Only name a header when nothing
but the proxy can reach the port: a header is forgeable by anyone who
can connect directly. Values that are not addresses are ignored in
favour of the peer, and IPv6 clients are keyed by their /64.

## What is here, and what isn't

```
crates/
├── flash-core     the scheduler wrapper, queue policy and domain types (no IO)
├── flash-store    SQLite: migrations, decks, cards, reviews, media, .apkg/CSV import and export
└── flash-server   the binary: web UI (askama + htmx), passkeys and passwords,
                   OAuth 2.1 + MCP, the JSON API the mobile apps use
```

The server exposes a small extension seam (`flash_server::ext`,
`flash_store::ext`) that the hosted product plugs its billing, sign-in
providers, plans and community into. None of that is in this repository,
and the core never depends on it: what you run here is complete.

## Contributing and support

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md)
for how the code is laid out, how to run the tests, and the contributor
license agreement. This is one person's project and the hosted service is
where the time goes, so bug reports get read and fixed as they come, and
feature requests are weighed against the roadmap rather than promised. If
you would rather not run a server, the hosted version is a sign-up away.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md)
for the embedded assets. Contributions are accepted under the CLA in
[CLA.md](CLA.md), which lets the same code power the hosted service.
