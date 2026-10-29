# Converge: agent guide and decision register

A self-hosted LLM gateway, built to replace LiteLLM in privacybox. One OpenAI
compatible endpoint in front of local (llama-swap) and cloud providers, with
per-person API keys, admin and user roles, usage tracking, and personas:
named system prompts that every client can pick like a model, for example
`image-prompt-helper`.

This file is the contract. Record decisions here when they are made, with the
date and the reason, and keep it true to the code. Never use em dashes.

**Status: contract approved 2026-10-29. M1 (skeleton, accounts, website shell), M2 (connections, models, keys, grants, `/v1` proxy, usage), M3 (personas), M4 (Responses API), M5 (Anthropic adapter), M6 (usage pages, limits, key scopes, metrics) M7 (privacybox app next to LiteLLM) built 2026-10-29, M8 (Playground) built 2026-10-30; next: the owner moves clients over.**

## Why it exists

LiteLLM problems that made us build our own (2026-10-29):

1. **Supply chain**: a compromised upstream image. Converge is built from
   source in our CI, every dependency pinned in `Cargo.lock` and checked by
   `cargo-deny`.
2. **Free tier limits**: governance features sit behind an enterprise license.
   Converge has no tiers.
3. **Latency**: noticeably slower than calling llama-swap directly. Converge
   keeps everything on the request path in memory and streams bytes through.

What we deliberately do not copy: LiteLLM's 100+ provider formats, its
response cache, the master key login on the admin UI, and database rows that
silently override the config file.

## Build and verify

Same shape as slopstream-server, kagami-web and quantifyme-v2: everything
through `make` (docker compose), no host toolchain.

| Target | What |
|---|---|
| `make dev` | bacon hot reload + Postgres 17 on `${PORT:-8080}` |
| `make test` | `cargo test --workspace --release` incl. Postgres integration tests |
| `make lint` | `cargo fmt --check` + `clippy -D warnings` + `cargo deny check` (the CI gate) |
| `make bench` | the latency benchmark against the stand-in upstream (see Performance) |
| `make image` / `up` / `down` | build and smoke-test the production image locally |
| `make admin EMAIL=..` / `make password EMAIL=..` | first account, then its password (stdin) |
| `make secret` | a fresh `CONVERGE_SECRET_KEY` |

CI: GitHub Actions. `ci.yml` runs the gate on every push; `release.yml` on a
`vX.Y.Z` tag runs the same gate, checks the tag equals
`[workspace.package] version`, pushes the amd64 + arm64 image
`ghcr.io/xblackbytesx/converge-server` and creates the GitHub release. The
GHCR package must be public (package settings, once). CI fetches cargo-deny
as a pinned release binary and checks its published sha256 before running
it; bump `v=` in both workflows together. The dev image builds it from
source (`cargo install --locked`).

Agent sandbox fallback (no Docker): rustup toolchain `1.92.0`, a scratch
`CARGO_TARGET_DIR`, and a throwaway Postgres 17 from the zonky
embedded-postgres binaries; point `CONVERGE_TEST_DATABASE_URL` at it.
Integration tests create and drop their own database and print `SKIP` when the
variable is unset. The sandbox can build cargo-deny from source into the
scratchpad but must not run downloaded release binaries. Real providers are
exercised by the owner; tests use stand-ins on real local ports
(`tests/common/upstream.rs`): an OpenAI-style upstream whose behaviour is
picked by the upstream model name (JSON, streams with and without usage,
slow headers, stall, crash mid-stream, endless, errors, audio), recording
what it received. An Anthropic-style one joins in M5. Never relax a test to
make it pass.

## Crates

| Crate | Role | Rule |
|---|---|---|
| `crates/core` (`converge-core`) | wire types, persona application, Anthropic translation (request, response, SSE), SSE usage scanner, key format, model name rules, limit arithmetic | Pure: no IO, no clock (time is passed in), no randomness |
| `crates/server` (`converge-server`) | axum + sqlx + askama/HTMX website, `/v1` proxy, upstream clients, usage writer, CLI | Runtime sqlx queries only (no `query!`), so builds never need a database |

Pins (same as the siblings): rust 1.92, axum 0.8, sqlx 0.8, askama 0.16,
reqwest 0.13 on rustls/ring, HTMX 2.0.4 vendored (`static/htmx.min.js`, sha256
`e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447`). Postgres
17. No Valkey: the server is single instance, so caches and counters live in
memory.

## Layout

- `core/src/secrets.rs`: secret prefixes (`cvg_` keys, `cvi_` invites, `cvs_`
  sessions) and the shape check; `core/src/names.rs`: public id rules shared
  by models and personas.
- `server/src/users.rs`: accounts, principals, invites; `auth.rs`: roles,
  secret minting and hashing; `limits.rs`: login throttling and the
  `/v1` limiter (state; the arithmetic is `core/src/limits.rs`);
  `metrics.rs`: Prometheus counters and the text format; `config.rs`:
  environment.
- `core/src/sse.rs`: the event splitter; `core/src/openai.rs`: endpoints,
  `model` rewrite, `include_usage`, usage parsing; `core/src/persona.rs`:
  the parameter list, `Params`, `apply_prompt` and `fill`;
  `core/src/responses.rs`: the Responses API, native persona rules and
  the translation over chat both ways; `core/src/anthropic.rs`: chat to
  Anthropic's Messages and back, streams included.
- `server/src/snapshot.rs`: the request path's snapshot and the shared
  upstream client; `proxy.rs`: `/v1`; `usage.rs`: the usage writer;
  `connections.rs`: connections, models and the shared public id
  namespace; `personas.rs`: personas and their history; `keys.rs`: keys,
  key scopes and grants; `crypto.rs`: sealing (from slopstream-server).
- `server/src/web/*`: pages. `connections.rs` (Connections for admins,
  Models for users), `personas.rs` (Personas, the persona editor),
  `playground.rs` (the Playground; its script is `static/playground.js`),
  `keys.rs` (Keys, Access and limits), `usage.rs` (Usage and its CSV).
- `migrations/`: numbered SQL, embedded and run at every start.

## Concepts

Two separate layers, each with its own pages, admin API and tables. Only the
OpenAI surface (`/v1/models`) puts them in one list, because the spec has no
other place for them.

- **Connection** (layer 1, the proxy): an upstream with a declared `kind`
  (`openai` or `anthropic`), a base URL, an encrypted credential (optional;
  llama-swap has none) and optional extra headers. The kind is always
  declared by the admin, never inferred from the URL: a LiteLLM or llama-swap
  URL proves nothing about what sits behind it.
- **Model** (layer 1): a public model id under one connection, mapped to that
  connection's own model name, e.g. `local/qwen3-30b` to llama-swap
  `qwen3-30b`, plus model-level defaults (such as Anthropic's `max_tokens`).
  Models are created by hand or imported from the connection's `/v1/models`
  (admin presses "Import"; imports are never automatic and never delete
  models). A model is pure plumbing: no prompt, no personality.
- **Persona** (layer 2, composed on top): a public id of its own, e.g.
  `image-prompt-helper`, with a system prompt and parameter defaults, and a
  reference to the model it currently runs on. The model is hot-swappable:
  the admin changes it at any time, the persona id stays the same, and every
  client keeps working without noticing. Clients cannot tell a persona from
  a model.
- **Principal**: whoever owns keys and receives access. Users now, teams
  later (see Teams).
- **Key**: a virtual API key belonging to a user, used as
  `Authorization: Bearer cvg_...`.

## Decisions

Settled 2026-10-29 (plan approved by the owner; do not reopen):

1. **Stack and repo**: Rust + HTMX in its own repo `/workspace/converge`, CI
   from day one, image published from our own CI only. GPL-3.0-or-later
   like slopstream-server (owner, 2026-10-29).
2. **OpenAI compatible on the outside, two kinds on the inside.** Clients only
   ever see the OpenAI API. Upstreams are `openai` (pass-through) or
   `anthropic` (translated). Gemini (its `/v1beta/openai/` endpoint),
   OpenRouter, OpenAI, Mistral, Groq, DeepSeek and llama-swap all go through
   `openai`. In use at approval: llama-swap, Gemini, OpenRouter, Anthropic,
   OpenAI. A third kind is added only when a
   provider's compatible endpoint is proven insufficient.
3. **Pass-through by default.** For `openai` upstreams the request body is
   parsed once (to resolve `model`, apply a persona, add `include_usage`),
   re-serialized with unknown fields kept as they are, and the response is
   streamed back byte for byte. Converge never normalizes upstream responses.
4. **No admin by environment.** There is no master key and no env login. The
   first admin is created with the CLI (`create-admin`, then `set-password`),
   like slopstream-server. Admin API calls use a key belonging to an admin.
5. **Nothing on the request path touches Postgres.** Keys, connections,
   models, personas and grants are loaded into an in-memory snapshot at start and swapped
   atomically after every admin change (same process, so no cross-instance
   invalidation is needed).
6. **Never store prompts or completions.** Usage rows hold counts, timings
   and status only. No request logging switch exists, so it cannot be left on
   by accident.
7. **No response cache.** Rejected 2026-10-29: it only pays off for repeated
   identical requests, and it was one of the moving parts that made LiteLLM's
   settings confusing.
8. **Teams are designed in, not built** (see Teams).
9. **Connections and personas are separate products in the UI** (owner,
   2026-10-29). "Connections" (each with its models) and "Personas" are
   dedicated pages with their own admin endpoints
   (`/api/v1/connections`, `/api/v1/models`, `/api/v1/personas`) and tables.
   They meet only in `/v1/models`, where each entry says which it is (see
   Request path).
10. **Personas point at a model by id, never by name**, so renaming or
    swapping a model is one write to the persona and nothing else.

## Request path (`/v1`)

1. Read the bearer token; reject anything not shaped `cvg_` + 43 base64url
   chars before hashing. SHA-256 it, look it up in the snapshot. Unknown,
   revoked or expired keys: `401` in OpenAI error format.
2. Parse the body as `serde_json::Value`, read `model`, resolve it to a
   persona or model in the snapshot, check the key's principal has access.
   No access looks the same as no such model: `404 model_not_found`.
3. Apply the persona (see Personas), rewrite `model` to the upstream name.
   Then the limits (M6): parallel requests per key, tokens per day,
   requests per minute, all in memory; over one is `429` with
   `Retry-After`.
4. Send upstream on that connection's pooled client. A client disconnect drops
   the upstream request, which stops generation on llama-swap.
5. Stream the response back. Streams are split into events (`core::sse`)
   and each is forwarded the moment its blank line arrives, byte for byte;
   an event is held only until it is complete (M2 plan, Proxy). Usage is
   read from the events or, for JSON, from the whole answer.
6. Hand a usage record to the usage writer (bounded channel, batched inserts).
   If the channel is full the record is dropped and counted in metrics; the
   request is never slowed down for bookkeeping.

**Usage on streams**: when the client streams without
`stream_options.include_usage`, Converge adds it upstream and filters that one
extra chunk (empty `choices`) out of what the client receives, because some
clients choke on it. When the client asked for it, it passes through.

**Endpoints, first version**: `POST /v1/chat/completions`,
`GET /v1/models`, `GET /v1/models/{id}`, and model-routed pass-through (body
has `model`, models on `openai` connections only) for `/v1/embeddings`, `/v1/completions`,
`/v1/rerank`, `/v1/images/generations`, `/v1/audio/speech`. `/v1/responses`
follows in M4 (see Responses API). Not yet: multipart audio transcription,
realtime, batches, files.

**Timeouts** (per connection, defaults): connect 10 s; first byte 600 s
(llama-swap may be loading a model); between stream chunks 120 s; no total
limit. Our own timers (`proxy.rs`): reqwest's read timeout would also cover
waiting for headers and cut off a model load, so it is not used.

**Errors**: upstream errors pass through with their status and body for
`openai`; `anthropic` errors are mapped to OpenAI's error shape. Converge's own
errors always use OpenAI's shape so every client shows them properly.

**`GET /v1/models`** lists the models and personas this key may use: models
first, then personas, each group sorted. `owned_by` is the connection name
for a model and `persona` for a persona, and every entry carries an extra
`converge` object (`{"kind": "model"}` or `{"kind": "persona", "title": ..,
"description": ..}`); OpenAI clients ignore unknown fields. That is how
personas show up in every client's model picker while staying a separate
concept.

## Personas (`core::persona`)

A persona has: `name` (public id, unique across models and personas, checked
in one shared namespace), `title`, `description`, `model_id`,
`system_prompt`, `prompt_mode`, parameter `defaults`, parameter `locks`, and
who may use it.

- **Swapping the model** is a dropdown on the persona page. It takes effect on
  the next request (snapshot swap); requests already streaming finish on the
  old model. Every swap is kept in `persona_history` (when, who, from, to) so
  "the designer got worse on Tuesday" can be traced, and the page has a
  "switch back" button.
- **Persona defaults and locks apply after the model's defaults**, so a
  persona's settings stay put when its model is swapped. If a lock names a
  parameter the new model's connection kind cannot take, the swap still
  saves and the page shows the warning.
- **A model in use by a persona cannot be deleted**; its page lists the
  personas on it and offers to move them first.
- **Access is separate**: a grant on a persona is enough to use it, without a
  grant on its model. So an admin can offer `designer` on an expensive model
  without opening the raw model to everyone.

- **`prompt_mode`**: `prepend` (default: the persona prompt goes first,
  client system messages stay after it) or `replace` (client system messages
  are dropped). Chosen per persona because some clients always send their own
  system prompt.
- **`defaults`** (temperature, top_p, max_tokens, and so on) fill in only what
  the client left out. **`locks`** always win, for personas that only work
  with, say, a low temperature.
- **Visibility of the prompt**: users see title and description in the
  website; whether they can read the prompt itself is a per-persona switch
  (default: readable).
- Personas never chain to other personas; `model_id` must be a model.
- On `anthropic` connections the persona prompt becomes the top-level `system`
  field, after translation.

## Responses API (`core::responses`, M4)

Owner, 2026-10-29: many clients (Codex CLI and newer agents) offer it next to
chat completions, so it is first class.

- **Chat completions is the internal shape.** Every translation goes through
  it: Responses to chat and back, chat to Anthropic and back. So Responses on
  an `anthropic` connection is two pure translations in a row, no third code
  path.
- **Native when the connection says so.** Each connection declares
  `responses: native | translate` (default `translate`; set `native` for
  OpenAI and, once checked, OpenRouter). Never probed: an upstream that 404s
  on `/v1/responses` today may add it tomorrow, and that is the admin's call.
- **Native**: pass-through like chat, the persona prompt goes in front of
  `instructions`; `previous_response_id` and stored responses work because
  the upstream stores them. Swapping a persona to a model on another
  connection breaks those ids; the swap page says so.
- **Translate**: stateless only, because Converge never stores conversations
  (decision 6). `input` items, `instructions`, function tools and calls, and
  images map to chat messages; the stream maps back to Responses events
  (`response.created`, `response.output_text.delta`,
  `response.function_call_arguments.delta`, `response.completed` with usage).
  `previous_response_id` gets a clear `400` naming the setting to use
  (`store: false`, send the full input). Built-in tools (web search, file
  search, computer use) are refused with `400` on translated connections.
- Golden tested both ways, streaming included, like the Anthropic adapter.

## Anthropic adapter (`core::anthropic`)

Pure translation, golden tested both ways: messages (text, images as base64
or URL, tool calls and tool results), `tools` / `tool_choice`, stop sequences,
`max_tokens` (required by Anthropic: taken from the request, else the model's
default, else 4096), the SSE stream (`message_start`, `content_block_*`,
`message_delta`) into OpenAI chunks, stop reasons, and usage including cache
tokens. Unsupported OpenAI parameters are dropped and named in a
`converge-dropped-params` response header rather than failing the request.

## Accounts and access

- **Roles**: `admin` (everything) and `user` (own keys, own usage, the models
  and personas granted to them). Roles live on the user row.
- **Grants**: a `grants` table of (principal, target) where target is a
  model, a persona, or `*`. A new user gets no grants unless the admin ticks
  "access to everything". Admins have implicit access to everything.
- **Keys**: `cvg_` + 32 random bytes base64url. Stored as SHA-256 plus the
  first 8 characters for display; shown once at creation. Fields: name,
  optional expiry, optional subset of the owner's grants, revoked flag.
  SHA-256 rather than Argon2 because the key itself has 256 bits of entropy
  and the hash sits on the hot path. `last_used_at` is written in batches by
  the usage writer.
- **Website login**: email + Argon2id password, server-side sessions in
  Postgres, cookie `HttpOnly; Secure; SameSite=Strict`. Cross-site writes:
  SameSite=Strict plus the Origin check in `web::guard` (every non-GET whose
  Origin names another host gets 403), as in slopstream-server; no per-form
  token, since both locks already have to fail for CSRF to work. Login
  attempts rate limited per address and per account
  (`limits.rs`: more than 10 failures per address or 5 per email within 60 s
  blocks it for 5 minutes, checked before Argon2 runs; only failures count,
  and accounts are never locked for good). The address is the socket peer,
  or with `CONVERGE_BEHIND_PROXY=true` the right-most `X-Forwarded-For`
  entry, the one Traefik appended. Declared, never guessed: off by default,
  because anyone can send that header.
  Invites are one-time links, like slopstream-server.
- **Connection credentials**: XChaCha20-Poly1305 under `CONVERGE_SECRET_KEY`,
  bound to the connection id. Never shown again after saving; the form only
  offers "replace".

## Teams (later, designed now)

Every key, grant and limit points at a **principal** (`principals` table:
`id`, `kind` = `user` | `team`), never at a user directly. Users get a
principal row on creation. Adding teams later means a `teams` table, a
`team_members` table, and resolving a key's effective grants as the union of
its user's and its teams'. No existing table changes shape.

## Usage and limits

- `usage` row per request: time, principal, key, persona, model, connection,
  upstream model, prompt / completion / cached tokens, time to first byte,
  total time, status, and whether usage was reported or estimated.
- Pages: own usage for users; everyone's (or one person's) for admins, by
  day, model or persona, key, person and connection, with a CSV (M6).
- Limits per person (M6): requests per minute, tokens per day, parallel
  requests per key, in memory, seeded from this table at start. A request
  turned away by a limit gets a usage row (status 429, `limited`). Cost
  budgets need a price table per model; parked until wanted.

## Website (askama + HTMX)

Navigation keeps the two layers apart: **Connections** and **Personas** are
separate top-level pages, never mixed in one list.

- **Connections** (admin): each connection with its models, "test
  connection", "import models", and per model the personas using it.
- **Personas** (admin): cards with title, description and the model each one
  currently runs on; the persona page has the prompt editor, the model
  dropdown, defaults and locks, the swap history, and a link into the
  Playground (M8), where it is tried, drafted and compared.
- **Playground** (everyone, M8): up to three streamed chat columns over
  any model or persona the viewer may use, timing and tokens per answer,
  and for admins persona drafts and the request as sent upstream.
- Admin also: users and invites, grants, keys, usage.
- User: own keys, own usage, and the same two pages read-only (Personas with
  descriptions, Models they can use). No external assets; strict CSP; same visual system as
slopstream-server.

## Performance

Target: Converge adds under 1 ms at p50 and under 3 ms at p99 to time to first
byte versus calling the upstream directly, measured by `make bench` against
the stand-in upstream on the same host. The benchmark runs in CI as a
regression check with generous bounds (p50 under 5 ms); real numbers are
recorded here.

| Date | Where | Direct p50 / p99 | Via Converge p50 / p99 | Added p50 / p99 |
|---|---|---|---|---|
| 2026-10-29 | agent sandbox, release build, 2 000 interleaved streaming requests | 0.15 / 0.27 ms | 0.37 / 0.56 ms | 0.22 / 0.36 ms |
| 2026-10-29 (M3) | same | 0.16 / 0.27 ms | 0.39 / 0.58 ms | 0.23 / 0.37 ms |
| 2026-10-29 (M4) | same | 0.14 / 0.28 ms | 0.35 / 0.57 ms | 0.21 / 0.37 ms |
| 2026-10-29 (M5) | same | 0.16 / 0.27 ms | 0.41 / 0.59 ms | 0.24 / 0.37 ms |
| 2026-10-29 (M6) | same, three runs, noisier host (direct p99 up too) | 0.18 / 0.37 ms | 0.44 / 0.73 ms | 0.24 / 0.46 ms |
| 2026-10-30 (M8) | same, two runs, quiet host | 0.13 / 0.23 ms | 0.33 / 0.53 ms | 0.20 / 0.35 ms |

## Security

- `cargo deny` for advisories, licenses, sources and wildcards, in the gate.
  Any advisory exception is an `ignore` entry in `deny.toml` with the id and
  why it cannot reach the running server. Open since 2026-10-29:
  RUSTSEC-2026-0374 (`basic-toml` unmaintained, compile-time only via
  askama_derive). Crates are `publish = false`.
- Runtime image: debian-slim, non-root uid 1000, read-only root filesystem
  supported, no shell needed at runtime.
- Body size limit (default 32 MiB, images in messages) and a request
  concurrency cap per key (`parallel_per_key`, default 16, per person).
- Keys and credentials never appear in logs; logs carry key prefix and user.
- `/metrics` (Prometheus) and `/healthz` are served on a separate internal
  port (`CONVERGE_METRICS_BIND`, default 9090), never through Traefik;
  optional bearer token `CONVERGE_METRICS_TOKEN`. The main port keeps its
  own `/healthz` for the compose healthcheck.

## M2 plan (approved by the owner 2026-10-29)

Goal: a client talks to llama-swap (and any other `openai` connection)
through Converge with its own key, within the latency target.

### Tables (`0002_proxy.sql`)

- `connections`: id, name, slug (default model prefix), kind (`openai` |
  `anthropic`, only `openai` offered until M5), base_url (includes `/v1`),
  credential (sealed, nullable), extra_headers (jsonb, sealed values),
  responses (`native` | `translate`, stored now, used in M4),
  first_byte_secs (600), chunk_secs (120), enabled.
- `models`: id, public_id (unique), connection_id, upstream_name, enabled.
  Personas join the same namespace in M3 (one check across both tables).
- `api_keys`: id, principal_id, name, key_sha256 (unique), display prefix,
  expires_at, revoked_at, last_used_at.
- `grants`: principal_id, target (`all` | model id); persona targets in M3.
- `usage`: at, principal_id, key_id, model_id, connection_id, endpoint,
  status, prompt / completion / cached tokens, ttfb_ms, total_ms,
  `usage_reported`, `client_closed`. Index (principal_id, at).

Deferred on purpose: per-key model subsets and the per-key concurrency cap
(M6, with limits), model-level defaults (M3, with personas).

### Snapshot (`snapshot.rs`)

One immutable `Snapshot` (keys by hash, models by public id, connections
with their ready HTTP client and opened credentials, grants per principal)
behind `arc-swap`, so a request does one atomic load and no locking. Every
admin write calls `reload()`, which rebuilds it from Postgres (tens of rows)
under a mutex with a generation counter, so two writes can never publish
out of order. Keys of disabled users, revoked or expired keys, disabled
models and connections are left out. Expiry is also checked per request.

### Proxy (`proxy.rs`, `core::sse`)

- Routes: `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings`,
  `/v1/rerank`, `/v1/images/generations`, `/v1/audio/speech`;
  `GET /v1/models`, `/v1/models/{id}`. Body limit 32 MiB, outside the web
  guard. Errors in OpenAI's shape.
- Order: key shape, hash, snapshot lookup (`401`); parse body once,
  resolve `model` and check grants (`404 model_not_found` for both cases);
  rewrite `model`; send.
- **To the upstream** only `content-type`, `accept`, the connection's
  credential as `Authorization: Bearer` and its extra headers. The client's
  `Authorization`, cookies and everything else stay behind.
- **From the upstream** status, `content-type` and body. Never
  `set-cookie`. Upstream errors pass through as they are.
- **Timeouts** are our own timers, not reqwest's read timeout (that one
  also covers waiting for headers, which would cut off a llama-swap model
  load): connect 10 s, `first_byte_secs` around `send()`, `chunk_secs`
  around every chunk. Timeouts answer `504`, refused connections `502`, both
  in OpenAI's shape and naming the connection, never its URL.
- **Streams** pass through an SSE splitter (`core::sse`, pure, tested on
  chunk boundaries split at every byte) that forwards each event the moment
  its blank line arrives. That replaces "never delays a byte": an event is
  held only until it is complete, so the injected usage chunk can be removed
  when the client did not ask for it. Usage is read from that chunk.
- **Non-streaming JSON** is forwarded as is; usage is read from its
  `usage` object. Binary answers (speech) pass through untouched.
- **Client goes away**: dropping the response drops the upstream stream,
  which closes the connection (llama-swap stops generating). A drop guard
  still records usage with `client_closed`.
- One info log line per request: key prefix, user, model, status, ttfb,
  total. Never bodies.

### Usage writer (`usage.rs`)

Bounded channel (10 000), one task inserts in batches (every 1 s or 500
rows) and updates `last_used_at` per key once per batch. Full channel: drop
and count, never wait. On shutdown the task drains what it has.

### Website

- **Connections** (admin): add / edit / disable, credential field that only
  ever says "set" or "replace", "Test connection" (`GET {base}/models`,
  shows status and model count), "Import models" (checkbox list from the
  upstream, public id defaults to `{slug}/{name}`, editable; existing ones
  marked). Each model row: public id, upstream name, enable switch.
- **Models** (users): what their grants allow, by connection.
- **Keys**: create (name, optional expiry, shown once with a copy button
  and a ready `curl` line), revoke. Admins also see everyone's keys with an
  owner column and can revoke them.
- **People**: per person, grant "everything" or tick models.
- **Usage**: a plain table of the last 100 requests (yours; admins see
  everyone's). Charts and totals stay in M6.

### Tests and bench

- Stand-in upstream (`tests/common`): OpenAI-style, can answer JSON,
  stream N events, stall before headers or mid-stream, die mid-stream,
  return an error, report usage or not, and record what it received (to
  prove headers and keys never leak upstream).
- Integration tests: auth and grant failures, `/v1/models` per key,
  pass-through byte equality, usage chunk added and removed, both
  timeouts, client disconnect reaching the upstream, usage rows, snapshot
  reload after every admin write, credentials sealed in the database.
- `make bench`: real TCP, stand-in upstream, 2 000 streaming requests
  direct vs through Converge; prints p50 / p99 of the added time to first
  byte. CI runs it with a loose bound (p50 under 5 ms) so regressions show.

### New dependencies

reqwest 0.13 (`stream`, rustls on ring, as the siblings), rustls 0.23,
chacha20poly1305 0.10 (`crypto.rs` from slopstream-server), arc-swap,
futures-util, bytes. All MIT or Apache-2.0; `cargo deny` checks them.

### No Anthropic stopgap

Owner, 2026-10-29: Claude waits for the real adapter (M5). Anthropic's
OpenAI-compatible endpoint is not offered as an interim connection.

### Built (2026-10-29)

As planned, with these specifics:

- **Every admin write reloads before it answers**, including role changes,
  disabling and enabling people. If the reload itself fails (Postgres
  gone), the write is saved but the request path keeps the previous
  snapshot until the next successful reload; the handler answers 500.
- **Upstream calls**: one shared reqwest client (pool per host, no
  redirects, `tcp_nodelay`), so connection reuse survives reloads.
- **Compression** is on the website only, never on `/v1`.
- **Upstream crashing before its headers** is a 502 (`send` fails), not a
  cut stream; mid-stream failures and stalls abort the response.
- **Usage writer**: a batch that fails (a key, model or connection deleted
  while its request ran) is retried row by row with the missing ids unset.
- **Unknown `/v1/...` paths** get axum's plain 404 for now; `/v1/responses`
  arrives in M4.
- **Bench, as measured**: see Performance.

## M3 plan (approved by the owner 2026-10-29, both choices as recommended)

Goal: personas appear in `/v1/models` next to models, work in any OpenAI
client, and can be moved to another model at any time without clients
noticing.

### Tables (`0003_personas.sql`)

- `personas`: id, public_id (unique; one namespace with `models`, checked
  in both tables), title, description, model_id (`REFERENCES models ON
  DELETE RESTRICT`, so the database itself refuses deleting a model in
  use), system_prompt, prompt_mode (`prepend` | `replace`),
  prompt_visible (default true), params (jsonb, see below), enabled,
  created_at, updated_at.
- `models.params` (jsonb, default `{}`): model-level defaults, same shape.
- `persona_history`: persona_id, at, by (user), what (`created`, `model`,
  `prompt`, `settings`), from_model / to_model, and the previous prompt and
  params, so any change can be traced and undone.
- `usage` gains persona_id and persona_name; `model_name` stays the model
  that actually answered.
- Grants: `target_kind = 'persona'` (already allowed by `0002`).
  "Everything" covers personas too.

### Parameters (`core::persona::Params`)

A fixed list, each set per persona to *off*, *default* (fills in only when
the client left it out) or *lock* (always wins): `temperature`, `top_p`,
`max_tokens`, `presence_penalty`, `frequency_penalty`, `seed`, `stop`,
`reasoning_effort`. Models get the same list as defaults only (locks live
on personas).

Per parameter: a persona lock wins; else what the client sent; else the
persona's default; else the model's default; else nothing. So a persona's
settings survive a model swap, and a model's defaults still apply under
personas that leave a parameter alone. `max_tokens` and
`max_completion_tokens` count as one: when the client sent either, neither
default fills in, and a lock writes the one the client used (newer OpenAI
models refuse `max_tokens`).

### Applying the prompt (`core::persona::apply`, pure)

Chat completions only. A persona on any other endpoint (embeddings,
images, legacy completions...) gets `400` saying personas work with chat;
Responses joins in M4 through `instructions`.

- **prepend** (default): the persona prompt goes into the first message,
  merged into the client's own leading `system` (or `developer`) message
  when there is one: persona text, a blank line, then the client's.
  Merged, not added as a second system message: several llama.cpp chat
  templates (Gemma among them) refuse a second system message, or one that
  is not first. Array content gets the prompt as a leading text part.
- **replace**: every client `system` and `developer` message is removed;
  the persona prompt becomes the only one.
- Golden tests: both modes, both roles, string and array content, no
  messages at all, every parameter rule above.

### Snapshot and proxy

- Snapshot gains `personas` (public id to persona with its model, already
  resolved) and per-principal persona grants. A persona whose model or
  connection is disabled is left out (`404 model_not_found`, as any absent
  model) and shown as *paused* on its page.
- `forward` resolves a persona first, then a model: one lookup each, still
  no database. The upstream sees the persona's model's upstream name.
- `/v1/models`: models first, then personas, each sorted. Persona entries:
  `owned_by: "persona"`, `converge: {kind: "persona", title, description}`.
  Never the model behind it.
- A grant on a persona is enough; no grant on its model needed.

### Website

- **Personas** (everyone): cards with title, id, description, and the model
  each runs on (admins); *paused* when its model is off.
- **Persona page** (admin): id, title, description; model dropdown grouped
  by connection; prompt editor; mode; "users can read the prompt"; the
  parameter table (off / default / lock per row); enabled. Below it the
  history with "switch back" on model changes and "restore" on prompt
  changes, and a **Try it** box: one message through the persona, not
  streamed, answer shown as text, recorded as usage without a key.
- **Persona page** (users with access): title, description, the prompt if
  readable, and the id to put in `model`.
- **Connections**: each model row lists the personas on it; removing such a
  model or its connection is refused with their names ("move them first").
  The model row also gets its default parameters.
- **Access**: personas get their own checklist next to models.

### Tests

Pure: `apply` and `Params` golden cases. Integration: personas in
`/v1/models` per grant; persona grant without model grant works; prompt
and parameters arrive at the stand-in as specified; swap takes effect on
the next request with history written; switch back; deleting a model in
use is refused (website and database); a paused persona is a 404;
non-chat endpoints refused; usage rows carry persona and model; the
persona page hides the prompt from users when set unreadable; Try it.

### Two choices for the owner

1. **History covers prompt and settings edits too**, not only model swaps
   (the contract named swaps). Recommended: "the designer got worse on
   Tuesday" is as likely a prompt edit as a swap, and storing the previous
   prompt makes restore one click. These are admin-written prompts, not
   user conversations, so decision 6 is untouched.
2. **Personas are chat only** (plus Responses in M4). Recommended: a system
   prompt means nothing to embeddings or images, and quietly ignoring it
   would hide a mistake.

### Built (2026-10-29)

As planned, with these specifics:

- **Token limit field, declared per connection** (`connections.token_field`,
  `max_tokens` by default, `max_completion_tokens` for OpenAI). The plan
  left open which name a default or lock uses when the client sent
  neither; guessing from the URL would be inferring, so the connection
  form asks.
- **One namespace, two tables**: every model or persona insert and rename
  takes a transaction-scoped advisory lock and checks both tables, so two
  admins cannot claim one id at the same moment. Import adds all ticked
  models in one transaction or none.
- **Model defaults apply to direct model calls too**, on chat and legacy
  completions; personas are chat only (400 elsewhere).
- **History**: one row per kind of change in a save (model, prompt,
  settings); title, description, visibility and enabled are not tracked.
  Undo saves the earlier value as a new change, so it is in the history
  too. "Switch back" disappears when the earlier model has been removed.
- **Try it** runs through the same `proxy::dispatch` as `/v1`, as the admin
  with no key (`usage.key_id` NULL). A paused or disabled persona says so
  instead of calling the upstream.
- **Access** now replaces all of a principal's grants (models and personas)
  in one save; the page shows both lists.
- **Removing** a model or a connection that personas use is refused with
  their names; the database's `ON DELETE RESTRICT` backs that up.
- **Users** see only live personas they may use, never the model behind
  one; a persona page they may not use is a 404.

## M4 plan (approved by the owner 2026-10-29)

Owner answers: reasoning items yes, switchable per model (on by default);
no stored-response routes in M4.

Goal: `POST /v1/responses` works for models and personas on every `openai`
connection, natively where the connection says so and translated over chat
completions everywhere else, streaming included. Done when Codex CLI works
against a llama-swap model (translated) and an OpenAI model (native).

### Routing

- `Endpoint::Responses` (`/v1/responses`), resolved like chat: persona or
  model, grants, `404 model_not_found`. Usage rows get endpoint
  `responses`.
- The connection's declared `responses` setting picks the path: **native**
  posts to `{base}/responses` and passes the answer through; **translate**
  turns the request into chat completions, posts to
  `{base}/chat/completions` and turns the answer back. Never probed.
- Not served in M4: `GET`, `DELETE` and `cancel` on `/v1/responses/{id}`,
  and `input_items`. They carry no `model`, so Converge cannot tell which
  connection holds the response; they get a clear `400` saying to send the
  full input (`store: false`), which is what Codex does anyway.
  `previous_response_id` inside a native request still works: the body
  names the model, so it reaches the upstream that stored it.

### Native

- Pass-through as chat: `model` rewritten, everything else kept, answer
  byte for byte. No `include_usage` (Responses streams always end with
  usage in `response.completed`); usage is read from the answer's `usage`
  or from that event's `response.usage`.
- **Persona prompt** goes into `instructions`: prepend is persona text, a
  blank line, the client's instructions; replace sets `instructions` to the
  persona prompt and drops `system` and `developer` input messages.
- **Parameters** map to their Responses names: `temperature`, `top_p`,
  `max_tokens` as `max_output_tokens`, `reasoning_effort` as
  `reasoning.effort`. The penalties, `seed` and `stop` have no Responses
  field and are skipped; the persona page says so for native connections.
- The model dropdown notes that on native connections a swap to another
  connection breaks conversations continued with `previous_response_id`.

### Translate (`core::responses`, pure)

Request to chat:

- `instructions` becomes the leading `system` message; `input` as a string
  becomes one `user` message.
- Input items: messages (`input_text`, `output_text`, `refusal` as text;
  `input_image` with a URL or data URL as `image_url`), `function_call`
  items as assistant `tool_calls` (consecutive calls merged into one
  assistant message), `function_call_output` as `tool` messages.
  `reasoning` items are dropped (chat has no place for them).
- `tools`: function tools reshaped (`{type, name, parameters}` to
  `{type, function: {...}}`); `tool_choice` likewise; `parallel_tool_calls`
  kept. `max_output_tokens` goes out as the connection's token field,
  `reasoning.effort` as `reasoning_effort`, `text.format` as
  `response_format` (`json_object`, `json_schema`). `stream` kept, with
  `include_usage` always on.
- Then the persona and model defaults apply exactly as on chat: the same
  `apply_prompt` and `fill`, all eight parameters.
- Refused with `400` naming the field: `previous_response_id`,
  `conversation`, `item_reference` items, `background: true`, `prompt`
  templates, `input_file` and `file_id` images, and every non-function
  tool (`web_search`, `file_search`, `computer_use`, `code_interpreter`,
  `image_generation`, `mcp`, `local_shell`, `custom`), as the contract
  says. Accepted and ignored, listed in a `converge-dropped-params`
  response header: `store` (nothing is stored either way), `include`,
  `metadata`, `user`, `safety_identifier`, `prompt_cache_key`,
  `service_tier`, `truncation`, `text.verbosity`, `reasoning.summary`.

Answer back:

- JSON: one Response object. `id` `resp_` plus 32 random hex chars (made
  by the server, passed in: core has no randomness), item ids derived
  from it. Output items in order: `reasoning` (from `reasoning_content`
  or `reasoning`, when the upstream sends either), `message` with
  `output_text` (or `refusal`), then one `function_call` per tool call.
  `finish_reason: length` gives `status: incomplete` with
  `incomplete_details.reason: max_output_tokens`; usage mapped to
  `input_tokens`, `output_tokens`, cached and reasoning token details.
  `model` is the id the client sent (persona or model).
- Stream: a state machine fed chat chunks, giving Responses events with
  `event:` lines and a running `sequence_number`: `response.created`,
  `response.in_progress`, `output_item.added`, `content_part.added`,
  `output_text.delta`, `reasoning_text.delta`,
  `function_call_arguments.delta`, the matching `.done` events as each
  item closes, and `response.completed` (or `response.incomplete`) with the
  whole response and usage. The `[DONE]` sentinel is not forwarded. An
  error event mid-stream becomes `response.failed`; a cut or stalled
  upstream still cuts the client, as on chat.
- Upstream errors (non-2xx) pass through as they are: chat and Responses
  share OpenAI's error shape.

Pass-through answers (chat, native Responses) keep the upstream's own
`model` field, as decision 3 says; only translated answers are built by
Converge and name what the client asked for.

### Proxy

- Two more tap modes: translated stream (splitter, then the state
  machine; usage still read from the chat chunks) and translated JSON
  (whole answer kept, converted at the end; over the 16 MB tap limit it is
  a `502` rather than a broken body).
- Personas now work on chat and Responses; other endpoints keep the `400`.

### Tests

Pure golden cases: every request mapping and refusal above; JSON answers
with text, tool calls, reasoning, refusal and `length`; streams of text,
reasoning then text, two parallel tool calls with split argument deltas,
usage, and an error chunk; sequence numbers always increasing.
Integration against the stand-in (which gains a native `/v1/responses`
and tool-call and reasoning streams on chat): native pass-through with
usage from `response.completed`; translate JSON and stream; a persona in
both modes; `previous_response_id` refused on translate and passed on
native; the dropped-params header; the stored-response routes' `400`;
usage rows. Codex CLI against both connections is the owner's check.

### Two choices for the owner

1. **Local reasoning shows up as `reasoning` items** in translated answers
   (from llama.cpp's `reasoning_content`, OpenRouter's `reasoning`).
   Recommended: Codex shows the thinking, and dropping it would hide what
   the model did. The alternative is text only.
2. **No stored-response routes in M4** (retrieve, delete, cancel,
   input items). Recommended: serving them means remembering which
   connection made each response id, a table that grows with every
   request; Codex never calls them. They can come later if a client needs
   them.

### Built (2026-10-29)

As planned, with these specifics:

- **Reasoning items per model** (`models.reasoning_items`, on by default):
  a "reasoning" switch on each model row under Connections. Off leaves the
  upstream's reasoning text out of translated answers.
- **Reasoning comes out as raw `reasoning_text`**, not as a summary,
  because it is the model's own thinking. Codex shows it only with
  `show_raw_agent_reasoning = true` in its config.
- **Translated JSON answers are read whole** before the client gets
  anything, with the same between-chunk timer and the 16 MB limit, so a
  failure there is a clean `502`, not a cut body.
- **Translated streams** send `response.created` and
  `response.in_progress` as soon as the upstream answers with a stream;
  usage is still read from the chat chunks for the usage row.
- **Unknown request fields** on translated connections are dropped and
  listed in `converge-dropped-params`, not passed on: a newer Responses
  field would make a chat upstream refuse the whole request.
- **Requests refused before the upstream** (untranslatable fields, unknown
  models) write no usage row, as on chat.
- **Stored-response routes** check the key first (401 without one), then
  answer `400`.

## M5 plan (approved by the owner 2026-10-29)

Owner answers: summarized thinking behind the model's reasoning switch,
yes; prompt caching as a per-model "cache" switch, on by default
(`models.prompt_cache`).

Goal: Claude models on an `anthropic` connection work from any OpenAI
client, chat and Responses, tools and streaming included. Done when
Claude is usable from Open WebUI and Codex CLI with tools.

API facts this rests on (checked 2026-10-29): `POST {base}/messages`
with `x-api-key` and `anthropic-version: 2026-10-21`. On that version
`temperature`, `top_p` and `top_k` are refused on every model. Thinking
is adaptive (`thinking: {type: "adaptive"}`) with depth set by
`output_config.effort` (`low` to `max`); `budget_tokens` is refused on
current models. Forced `tool_choice` (`any`, `tool`) is refused by Claude
Opus 5.5, Sonnet 5.5 and Fable 5.1. Assistant prefill is refused on
current models.

### Connections

- The connection form gets **Kind**: OpenAI compatible or Anthropic.
  Declared, never inferred (decision 2). Base URL as usual, with the
  version: `https://api.anthropic.com/v1`.
- Anthropic connections send the credential as `x-api-key` (never
  `Authorization`) plus `anthropic-version`; extra headers still apply,
  so `anthropic-beta` is set there. `x-api-key` and `anthropic-version`
  join the reserved headers.
- Test and Import use `GET {base}/models?limit=1000` with the same
  headers, following `has_more`/`last_id` pages.
- They serve chat completions and Responses (always translated: the
  form offers no "native" for this kind). Every other endpoint gets
  `400` naming what the connection serves.
- The snapshot loads both kinds; `anthropic` stops being "paused".

### Request: chat to Messages (`core::anthropic`, pure)

- `system` and `developer` messages, wherever they are, become the
  top-level `system`, in order. The persona prompt is already merged into
  the first one (M3), so it ends up first in `system`, as the contract
  says.
- `user` and `assistant` text, images (data URLs as `base64` sources,
  http(s) URLs as `url` sources), assistant `tool_calls` as `tool_use`
  blocks (arguments parsed; invalid JSON is a `400`), and `tool`
  messages as `tool_result` blocks. Consecutive `tool` messages go into
  one user message, consecutive same-role messages are merged.
- `tools` reshaped (`parameters` to `input_schema`, `strict` kept);
  `tool_choice` `auto`/`none` map directly, `required` to `any`, a named
  function to `tool`, `parallel_tool_calls: false` to
  `disable_parallel_tool_use`. Models that refuse forced tool choice say
  so in their own error, passed on.
- `max_tokens` (or `max_completion_tokens`) as is; else the model's or
  persona's `max_tokens` setting; else 16 000, or 64 000 when streaming.
  The contract said 4096; that cuts current models off mid-answer.
- `stop` to `stop_sequences`, `reasoning_effort` to
  `output_config.effort` (`none` and `minimal` as `low`),
  `response_format` `json_schema` to `output_config.format`, `user` to
  `metadata.user_id`.
- Dropped and named in `converge-dropped-params`: `temperature`,
  `top_p` (refused by the API version), `seed`, the penalties,
  `logit_bias`, `logprobs`, `top_logprobs`, `response_format`
  `json_object`, and anything else unknown. `n` above 1 is a `400`. The
  persona page notes that only `max_tokens`, `stop` and
  `reasoning_effort` reach an Anthropic model.
- Thinking blocks are not sent back: OpenAI clients never keep them, and
  the API drops what it cannot read. Claude reasons afresh each turn.
  Checked against the live API before this is called done (tool loops
  with thinking on are where it could refuse).

### Answer: Messages to chat

- JSON: text blocks joined into `content`, `tool_use` blocks as
  `tool_calls` (input serialised as `arguments`), thinking text as
  `reasoning_content` (so M4's reasoning items work on Claude too).
  `model` is the id the client asked for. Stop reasons: `end_turn` and
  `stop_sequence` to `stop`, `max_tokens` and
  `model_context_window_exceeded` to `length`, `tool_use` to
  `tool_calls`, `refusal` to `content_filter`. Unknown block types are
  skipped.
- Usage: `prompt_tokens` is input plus cache reads plus cache writes,
  `cached_tokens` is the cache reads, `completion_tokens` the output.
- Stream: a state machine from Anthropic events to chat chunks:
  `message_start` gives the role chunk, `text_delta` content,
  `thinking_delta` `reasoning_content`, `tool_use` starts and
  `input_json_delta` give `tool_calls` deltas by index, `message_delta`
  the finish reason, then a usage chunk (removed again by the existing
  `include_usage` rule when the client did not ask) and `[DONE]`.
  `ping` and `signature_delta` are skipped; an `error` event becomes an
  error chunk and ends the stream.
- Errors: Anthropic's `{type: "error", error: {type, message}}` becomes
  OpenAI's error shape with the same status, except `529 overloaded` as
  `503` so OpenAI clients retry it.
- Responses on an Anthropic connection is the two translations in a row
  (Responses to chat to Messages, and back), as the contract planned:
  no third code path. In a stream the two state machines run chained.

### Tests

Pure golden cases both ways: every request mapping and drop above,
message merging, tool round trips, images, `max_tokens` fallbacks; JSON
answers with text, tools, thinking, each stop reason and cache usage;
streams of text, thinking then text, two tool calls with split JSON, and
an error event. Integration against the stand-in (which gains
`/v1/messages` and `/v1/models` in Anthropic's shape, checking
`x-api-key` and `anthropic-version` arrive and `Authorization` does not):
chat JSON and stream, Responses over Anthropic, a persona, import,
upstream errors mapped, non-chat endpoints refused. Claude with tools in
Open WebUI and Codex is the owner's check.

### Two choices for the owner

1. **The model's "reasoning" switch asks Claude for summarized
   thinking** (`thinking: {type: "adaptive", display: "summarized"}`) on
   Anthropic models. Recommended: current Claude models think anyway but
   return empty thinking text by default, so without this there is
   nothing to show. Off sends no `thinking` at all (the model's default).
2. **Prompt caching on by default** (top-level `cache_control:
   {type: "ephemeral"}` on every request). Recommended: agent clients
   resend the whole conversation every turn and persona prompts are a
   stable prefix, so most input becomes cache reads at a tenth of the
   price; a cache write costs a quarter more than plain input once. Off
   means no caching unless a client asks itself (it cannot, through the
   OpenAI API).

### Built (2026-10-29)

As planned, with these specifics:

- **Model switches on Anthropic connections**: "reasoning" asks for
  summarized adaptive thinking, "cache" adds top-level `cache_control`
  (`models.prompt_cache`, default on). The cache box shows only on
  Anthropic connections; elsewhere it keeps its value.
- **Kind is editable** on an existing connection (declared, like
  everything else); an Anthropic connection's Responses setting is saved
  as translate whatever the form sends.
- **Usage**: a translated answer's usage is read from the chat shape, so
  usage rows count cache reads and writes in prompt tokens, cache reads
  as cached tokens. The usage row keeps the upstream's own status (`529`
  stays `529` there; the client gets `503`).
- **Errors and non-streamed answers** are read whole before answering, so
  they can be translated; streams are translated event by event.
- **A cut Anthropic stream** (no `message_stop`) ends without `[DONE]`
  (or without `response.completed`), so clients can tell it was cut.
- **Refused before the upstream** (untranslatable fields, endpoints an
  Anthropic connection does not serve) writes no usage row, as on chat.
- **Not verified here**: the live API. The tool loop with thinking on
  (thinking blocks not sent back) is the first thing to check with Claude
  in Codex.

## M6 plan (approved by the owner 2026-10-29, both choices as recommended)

Goal: people can see what they used, admins can cap it, and Prometheus can
watch the gateway. Also the two pieces M2 deferred to here: per-key model
subsets and the per-key parallel cap. Done when the owner reads a week of
usage by day and by model, a limit turns a client away with a `429` that
Open WebUI and Codex show properly, and Prometheus scrapes the metrics port.

No new dependencies: the metrics text format is written by hand.

### Tables (`0006_limits.sql`)

- `users`: `rpm` (requests per minute), `tokens_per_day`, `parallel_per_key`
  (default 16). `NULL` means no limit. Limits sit on the user row now and
  move to `principals` with teams; nothing else changes shape.
- `key_scopes` (key_id, target_kind `model` | `persona`, target_id). No rows:
  the key may use everything its owner may. Rows: only those, and only while
  the owner still has the grant (the effective set is the intersection, so
  taking a grant away from a person also takes it from their keys).
- `usage`: `limited` text (`rpm`, `tokens`, `parallel`; `NULL` otherwise),
  only if the owner says yes to choice 2.

### Limits (`core::limits`, pure; state in `limits.rs`)

All in memory, checked after the model or persona resolves and before
anything goes upstream. Nothing on the request path touches Postgres.

- **Requests per minute**: a token bucket per person (holds `rpm`, refills
  `rpm / 60` per second), so a burst at a window edge cannot double the
  rate. `Retry-After` is the time until one request is free again.
- **Tokens per day**: prompt plus completion tokens (cached reads included:
  they are tokens the model handled), counted per person for the current
  day (choice 1). Tokens are known only after an answer, so the check is
  "already at or over the limit": the request that crosses the line
  finishes, the next one gets `429`. At start the counters are seeded from
  today's usage rows (one query), so a restart does not hand out a fresh
  day.
- **Parallel requests per key**: a counter per key, released when the
  answer ends or the client goes away (the usage guard's drop). Counted per
  key, so one runaway script cannot starve the person's other clients.
- **Who**: limits apply to whoever has them set, admins included; the
  admin's own row has none unless set. Try it on the website counts towards
  the admin's requests and tokens; it has no key, so no parallel cap.
- **The answer**: `429` in OpenAI's shape, code `rate_limit_exceeded`,
  with `Retry-After` (seconds) and a message naming the limit and when it
  frees up, e.g. "Tokens per day: 200 000 used of 200 000, resets at 00:00
  Europe/Amsterdam". OpenAI clients retry on `429` with backoff; Codex
  waits for `Retry-After`.
- The limiter state is keyed by person and key id and survives snapshot
  reloads; changing a limit takes effect on the next request.

### Website

- **People > Access** (admin) gains the three limits next to the grants.
- **Keys**: creating a key offers "Everything you can use" (default) or a
  checklist of the person's models and personas. The key list shows the
  scope ("all", or the names).
- **Usage** becomes a page with a period (today, 7 days, 30 days, or a
  month; default 7 days), and for admins a person filter:
  - totals: requests, errors, tokens in (with the cached share), tokens
    out, median time to first byte;
  - per day: requests and tokens as a plain CSS bar per row (no
    JavaScript, no chart library, CSP unchanged);
  - by model or persona, by key, and for admins by person and by
    connection;
  - "today" against the person's limits when they have any ("12 400 of
    200 000 tokens");
  - the recent requests table stays, filtered by the period, with an
    "Older" link (keyset on `id`);
  - **Download CSV** of the rows in the period, same filters, streamed.
  Days are the viewer's own time zone (`users.tz`). Users see only their
  own; admins everyone's. Queries run on the `usage` table through the two
  existing indexes; daily rollups only if that ever gets slow.
- Usage rows are kept; no pruning in M6 (a row is about 150 bytes).

### Metrics port

- `CONVERGE_METRICS_BIND`, default `0.0.0.0:9090`, `off` to disable.
  Serves `/metrics` (Prometheus text format) and `/healthz`; the main port
  keeps its `/healthz` for the compose healthcheck. Optional
  `CONVERGE_METRICS_TOKEN`: when set, `/metrics` wants it as a bearer
  token, for when the port is published on the LAN for a Prometheus on
  another host.
- Recorded in one place, the usage guard, next to the usage record:
  - `converge_requests_total{endpoint, model, persona, status}` (status as
    class: `2xx`, `4xx`, `5xx`, plus `limited`);
  - `converge_tokens_total{model, kind}` (prompt, completion, cached);
  - `converge_ttfb_seconds` and `converge_request_seconds` histograms per
    connection (buckets 0.05 s to 600 s, model loads included);
  - `converge_inflight_requests{connection}`;
  - `converge_upstream_failures_total{connection, reason}` (connect,
    timeout, cut stream);
  - `converge_limited_total{limit}`;
  - `converge_usage_dropped_total`, `converge_usage_queue`;
  - `converge_snapshot_reloads_total`, `converge_build_info{version}`.
- No person or key labels: privacy, and it keeps the series count bounded
  by the models and personas the admin created.

### Tests and bench

- `core::limits` unit tests: bucket refill and `Retry-After`, day rollover
  in a time zone (the clock is passed in), the "at or over" rule.
- `limits_pg.rs`: rpm and tokens per day give `429` with `Retry-After` and
  nothing reaches the upstream; the parallel cap with a slow stream held
  open; counters seeded after a restart; key scopes (allowed, refused as
  `model_not_found`, grant taken away from the person).
- `web_pg.rs`: usage periods, by model and persona, users see only their
  own, CSV, limits saved on Access.
- `metrics_pg.rs`: a scrape after a few requests has the counters; the
  token is enforced when set; the main port does not serve `/metrics`.
- Bench: the limiter is on the request path, so the run is repeated and
  recorded; the target stays under 1 ms added at p50.

### Two choices for the owner

1. **Tokens per day is a calendar day in the person's own time zone**,
   resetting at their midnight. Recommended: it matches the days on the
   Usage page and is one counter per person. The alternative, a rolling
   24 hours, needs per-hour buckets and an answer like "frees up
   gradually", which is harder to read.
2. **Requests turned away by a limit get a usage row** (status `429`, zero
   tokens, a "limit" badge on the Usage page). Recommended: otherwise a
   person whose client failed sees nothing about why. It is the one
   exception to "refused before the upstream writes no row"; other
   refusals (unknown model, untranslatable field) stay unrecorded.

### Built (2026-10-29)

As planned, with these specifics:

- **Order on the request path**: limits are checked after the request
  translates (persona applied, Anthropic body built), right before the
  upstream call, so a request refused for its content uses up nothing.
  Order inside: parallel, tokens, then requests per minute (the only one
  that takes something); a refusal takes nothing.
- **Tokens per day** are counted for everyone, limit or not, so a limit
  set mid-day starts from what the day already used. A changed time zone
  (Settings) reloads the snapshot; the count follows the new day.
- **Scoped keys** carry `api_keys.scoped`: deleting everything in a scope
  leaves a key that reaches nothing, never everything. Deleting a model,
  connection or persona deletes its scope rows with its grants. On the key
  form the scope is a checklist under "Only some models and personas";
  nothing ticked is everything.
- **Access form**: limits accept spaces in numbers (`200 000`); empty is
  no limit, 0 is refused. The per-key parallel cap defaults to 16 for
  every person, existing ones included.
- **Usage page**: tiles (requests, errors including limits, tokens in
  with the cached share, tokens out, median first byte), per day as a
  `<progress>` bar (no inline style, CSP unchanged), breakdowns, today
  against the person's limits (users, or admins filtering one person),
  recent requests with "Older" paging, CSV (`/usage.csv`, paged by id,
  formula-leading fields defused). Days in the viewer's time zone.
- **Metrics**: `status` label is the class (`2xx`, `4xx`, `5xx`,
  `limited`); upstream failure reasons are `connect`, `request`,
  `timeout`, `stalled`, `broke off`, `unreadable`, also in the log line's
  `upstream` field. `converge_snapshot_reloads_total` is the snapshot
  generation. Limited requests are counted, not timed.
- **Not verified here**: Prometheus itself scraping, and how Open WebUI
  and Codex show the `429`.

## M7 (built 2026-10-29)

The privacybox app, next to LiteLLM. Small enough that it was built
straight from the milestone row, on the owner's go-ahead.

- **`apps/converge-server`** in privacybox (named after the image and
  `deploy/`, like `apps/slopstream-server`): `docker-compose.yml` on the
  stack's conventions (proxy + internal networks, bind volume under
  `DOCKER_ROOT`, the four file middlewares, `cap_drop: ALL`, read-only
  root) and `.env.example`. `CONVERGE_BEHIND_PROXY` is fixed to `true`
  there: it always sits behind that Traefik. The metrics port is not
  published.
- **`docs/converge.md`**: first run, connections by FQDN, moving a client
  off LiteLLM one at a time, ports, verification, updates. README rows
  added (README left unstaged: it carries the owner's own edits); the
  app and doc are staged, never committed.
- **Side by side**: separate subdomain and database, nothing shared. A
  client moves by changing base URL and key; model ids can be kept by
  adding models by hand under the ids clients already send.
- **Not covered**: clients that need Anthropic's own API (Claude Code via
  `ANTHROPIC_BASE_URL`, as in privacybox's Paperclip example) cannot move;
  Converge serves the OpenAI API only. They stay on LiteLLM or go direct.
  Serving `/v1/messages` to clients would be its own milestone.
- **Not verified here**: the compose file under Docker and the switch
  itself (the done-when is the owner moving clients).

## M8 plan: Playground (approved by the owner 2026-10-30, all three choices as recommended)

Goal: a chat page on the website for testing connections, tuning personas
and comparing model and persona combinations, with timing and token
figures for every answer. Done when the owner tunes a persona in the
Playground, saves it, and compares it on two models side by side.

### What it is

- **Playground** in the navigation (`/playground`). Up to three columns
  side by side, each with its own picker (any model or persona the viewer
  may use) and its own conversation. One message box at the bottom sends
  to every column at once, or per column. Add or remove a column, clear
  one, or copy one column's conversation into another to branch it.
- **Per column settings**: the persona parameter list (temperature, top_p,
  max_tokens, ...) as the client's own values, so a persona's locks and
  defaults behave exactly as for a real client. An optional system prompt
  of the column's own, sent as a client would send one.
- **Streamed**, with a Stop button (stops generation upstream, recorded as
  "client went away"). Answers render as plain text with code blocks kept;
  reasoning (`reasoning_content`) shows folded above the answer.
- **Shortcuts in**: "Open in Playground" on each model (Connections) and
  each persona page, which preselects it.

### Tuning a persona (admins)

A persona column can switch to **draft**: the persona's prompt, prompt
mode, parameters and model become editable in the column, and requests use
the draft instead of what is saved. That is how one persona is tried on
another model, or two prompt versions are compared side by side (two
columns, same persona, two drafts). **Save to persona** writes the draft
through the normal persona update, so it lands in the persona's history
and can be undone there. Drafts live in the page only.

### Diagnostics, per answer

- Time to first byte and to the first token (from send, as the browser
  sees it), total time, and output tokens per second (completion tokens
  over the time after the first token).
- Tokens in (cached), out, and reasoning when the upstream reports them;
  finish reason; status.
- Which model answered (a persona names its model), parameters dropped
  for that upstream (`converge-dropped-params`), and for admins the
  request body as it went upstream (merged system prompt, the parameters
  that won, the Anthropic body after translation), folded.
- Per column: totals for the conversation so far.

### How it works

- The browser sends the whole conversation each turn to
  `POST /playground/send` (session cookie, the website's Origin check),
  which runs `proxy::dispatch` exactly like `/v1` and Try it, streaming,
  with usage asked for. The caller is the viewer without a key, like Try
  it, so their grants and limits apply (no per-key parallel cap). A draft
  becomes a one-off persona entry for that request; the snapshot is never
  touched.
- `dispatch` hands back what it sent upstream and the answering model in
  a response extension (never on the wire); the playground handler puts
  it in a first SSE event (`event: converge`) ahead of the upstream's own.
- Nothing is stored on the server (decision 6). Conversations sit in the
  page, in `sessionStorage` so a reload keeps them; closing the tab drops
  them. Usage rows are written as for every request, shown as "Playground"
  on the Usage page (key column), with Try it.
- One new static file, `static/playground.js` (plain JavaScript, no
  library, `fetch` with a streamed body; CSP unchanged: `script-src 'self'`,
  `connect-src 'self'`). No build step.

### Not in M8

Images and files in messages, tools, the Responses API as the wire (the
Playground speaks chat; Responses is covered by its tests and Codex),
saving or sharing conversations.

### Tests

- `playground_pg.rs`: a model and a persona stream through `/playground/send`
  with the `converge` event first; a draft overrides prompt, params and
  model without touching the saved persona; users get only what they are
  granted, never drafts, never the upstream body of a persona whose prompt
  is hidden; the Origin check and login apply; limits apply and the row
  says Playground.
- `web_pg.rs`: the page renders for both roles with no inline script or
  style; "Open in Playground" links preselect.
- The JavaScript is checked by hand in a browser by the owner (no browser
  in the sandbox's test gate).

### Three choices for the owner

1. **Everyone gets the Playground**, limited to their own models and
   personas and counted against their own limits; drafts and the upstream
   body are admin only. Recommended: users want to try a persona before
   wiring a client to it. The alternative is admins only.
2. **Up to three columns.** Recommended: two covers "same prompt, two
   models", three covers "before, after, and another model", and more
   gets too narrow to read. The alternative is a single conversation.
3. **The persona page's Try it box goes**, replaced by "Open in
   Playground". Recommended: one place to try things, and the Playground
   does all Try it did plus streaming and timing.

### Built (2026-10-30)

As planned, with these specifics:

- **Routes**: `GET /playground` (`?pick=<id>` preselects; the links on
  Connections, Models and persona pages use it), `POST /playground/send`
  (JSON, 4 MiB body limit instead of the website's 64 KiB, since it
  carries the whole conversation), and for admins
  `GET|POST /playground/persona/{id}` (a persona's saved values to start a
  draft; saving a draft). JSON answers use the `/v1` error shape.
- **Page data** goes to `playground.js` as JSON in a data attribute, so
  there is still no inline script. `static/playground.js` is served like
  `app.js`.
- **`proxy::dispatch` takes `Extras`** (accept header, `inspect`, a
  draft). With `inspect` it keeps a copy of the body as sent and returns
  it with the answering model, upstream name, connection and dropped
  fields as a `Sent` response extension. `/v1` never sets it, so the
  request path copies nothing.
- **Column settings** are parsed with the persona form's own rules and
  sent as the client's values; `max_tokens` goes out as the connection's
  token field, as a client set up for it would send.
- **An upstream that answers whole** although asked to stream (some do)
  comes back as JSON with the same information and shows as a normal
  answer.
- **Users** see which persona answered and what was dropped; never the
  model behind a persona, never the body. Drafts and their routes are
  403 for them.
- **Try it is gone**: its handler, template and route. The persona form
  keeps a "Try it" card that links to the Playground. Usage rows without
  a key show as "Website (Playground)" (old Try it rows too).
- **Timing** is measured in the browser: first byte is when the stream
  starts (Converge sends its `converge` event once the upstream answered
  with headers), first token is the first content or reasoning delta.
- **Not verified here**: the page in a real browser. The server side is
  covered by `playground_pg.rs`; the JavaScript passed a syntax check
  only.

## Milestones

| | Scope | Done when |
|---|---|---|
| M1 | Workspace, make, Docker, CI, migrations, CLI admin, login, invites, website shell | admin logs in, invites a user |
| M2 | Connections, models, keys, grants, snapshot, `openai` pass-through, `/v1/models`, usage writer | a client talks to llama-swap through Converge within the latency target |
| M3 | Personas page, model swap with history, persona grants | a persona appears and works in Open WebUI and one other client |
| M4 | Responses API: native pass-through + translation over chat | Codex CLI works against a llama-swap model and an OpenAI model |
| M5 | Anthropic adapter (chat, and Responses through it) | Claude models usable from any OpenAI client, tools included |
| M6 | Usage pages, limits, per-key scopes, metrics port (built 2026-10-29) | a week of usage reads by day and model, a limit gives a `429` clients show, Prometheus scrapes |
| M7 | privacybox `apps/converge-server` + docs, LiteLLM side by side (built 2026-10-29) | the owner switches clients over |
| M8 | Playground: chat columns, persona drafts, per-answer timing (built 2026-10-30) | the owner tunes and saves a persona there and compares it on two models |
| Later | Teams, cost budgets, transcription | |

## Settled answers (2026-10-29)

- License GPL-3.0-or-later.
- Providers in use: llama-swap, Gemini, OpenRouter, Anthropic, OpenAI. All
  but Anthropic are `openai` connections, so the Anthropic adapter stays
  after the Responses API.
- `/v1/responses` is wanted: milestone M4.
- Persona prompts are readable by users by default (per-persona switch).
