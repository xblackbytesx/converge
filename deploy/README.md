# Deploying Converge

The released image `ghcr.io/xblackbytesx/converge-server` behind Traefik,
with Postgres 17. Tags: `X.Y.Z`, `X.Y`, `X`, `latest`.

## Files

Copy `docker-compose.yml` and `../.env.example` (as `.env`) into your stack.

```sh
mkdir -p $DOCKER_ROOT/converge-server/database
```

Everything the server keeps lives in Postgres. The app has no volume and
runs with a read-only root filesystem.

## Configuration

| Variable | Required | Meaning |
|---|---|---|
| `DATABASE_URL` | yes | Postgres connection string |
| `CONVERGE_SECRET_KEY` | yes | 32 random bytes, base64. Encrypts connection credentials at rest |
| `CONVERGE_PUBLIC_URL` | no | The address people use, for invite links. Default: the request's host |
| `CONVERGE_BEHIND_PROXY` | no | `true` behind Traefik: login throttling uses the client address Traefik adds to `X-Forwarded-For`. Default off (the header is ignored) |
| `CONVERGE_BIND` | no | Listen address, default `0.0.0.0:8080` |
| `CONVERGE_COOKIE_SECURE` | no | `false` only for plain-http LAN use (default on) |
| `CONVERGE_METRICS_BIND` | no | Prometheus `/metrics` and `/healthz`, default `0.0.0.0:9090`; `off` to disable |
| `CONVERGE_METRICS_TOKEN` | no | When set, `/metrics` wants `Authorization: Bearer <token>` |
| `RUST_LOG` | no | Log level, default `info` |

Generate the secret once and keep a copy with your backups:

```sh
head -c 32 /dev/urandom | base64
```

Losing or changing it makes every saved connection credential unreadable;
they then have to be entered again. Accounts, keys and personas are not
affected.

## DNS and ports

- One host name, e.g. `converge.example.com`, routed by Traefik to port 8080.
  The website and the OpenAI-compatible endpoint share it.
- The router uses the `https-redirect`, `non-www`, `secure-headers` and
  `gzip-compress` file middlewares. The server sets its own
  Content-Security-Policy; if `secure-headers` also sets one, the stricter
  combination applies and must still allow `script-src 'self'` and
  `connect-src 'self'`.
- Postgres stays on the internal network.
- Port 9090 (metrics) is never routed by Traefik and not published by the
  example compose file. For a Prometheus on another host, publish it on a
  LAN address only (`192.168.1.10:9090:9090`) and set
  `CONVERGE_METRICS_TOKEN`.

## First run

```sh
docker compose up -d
docker compose exec converge-server-app converge-server create-admin you@example.com
docker compose exec -T converge-server-app converge-server set-password you@example.com
```

`set-password` reads one line from stdin. There is no master key and no
login from environment variables: this CLI is the only way to the first
admin. Then log in and invite people under People.

## Using it

1. Connections, Add connection. Base URLs include the version segment:

   | Upstream | Base URL | Key |
   |---|---|---|
   | llama-swap | `http://llama-swap:8080/v1` (or its FQDN) | none |
   | OpenAI | `https://api.openai.com/v1` | yes |
   | OpenRouter | `https://openrouter.ai/api/v1` | yes; optional `HTTP-Referer` and `X-Title` headers |
   | Gemini | `https://generativelanguage.googleapis.com/v1beta/openai` | yes |
   | Anthropic | `https://api.anthropic.com/v1`, kind "Anthropic" | yes |

   Pick `max_completion_tokens` as the token limit field for OpenAI, and
   Responses API "Native" for OpenAI; leave the rest on "Translate".
2. Import models, then People, Access to grant them (admins need no grants).
3. Personas, New persona: a title, an id, a model and a prompt. Grant it
   under Access like a model; a persona needs no grant on its model.
4. Keys, Create key. Clients use base URL `https://converge.example.com/v1`
   and that key. Tick models or personas under "Only some" for a key that
   reaches only those.
5. Limits per person sit under People, Access: requests per minute, tokens
   per day (reset at the person's midnight) and parallel requests per key
   (16 by default). Clients over a limit get `429` with `Retry-After`.

Long streams need no proxy tuning: Traefik's defaults set no write timeout.
Converge cuts an upstream that sends nothing for the connection's "seconds
between chunks" (120 by default).

## Verification

```sh
docker compose ps                                     # both containers healthy
curl -fsS https://converge.example.com/healthz        # ok
curl -fsS https://converge.example.com/v1/models -H "Authorization: Bearer cvg_..."
docker compose exec converge-server-app curl -fsS localhost:9090/metrics | head
docker logs converge-server-app 2>&1 | tail -20
```

## Upgrades

Migrations run at start. Pin `CONVERGE_VERSION` to a release, pull, restart.

## Backups

Back up the Postgres database and keep `CONVERGE_SECRET_KEY` with it.
