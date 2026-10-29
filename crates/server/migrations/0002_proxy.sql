-- Connections and their models (layer 1), API keys, grants and usage.

CREATE TABLE connections (
    id              uuid PRIMARY KEY,
    name            text NOT NULL UNIQUE,
    -- Default prefix for imported model ids (`local` -> `local/qwen3-30b`).
    slug            text NOT NULL UNIQUE,
    kind            text NOT NULL DEFAULT 'openai' CHECK (kind IN ('openai', 'anthropic')),
    -- Up to and including the version segment: https://api.openai.com/v1
    base_url        text NOT NULL,
    -- XChaCha20-Poly1305 under CONVERGE_SECRET_KEY (crypto.rs); NULL: none.
    credential      bytea,
    -- Sealed JSON list of [name, value] pairs; NULL: none.
    extra_headers   bytea,
    -- How /v1/responses reaches this upstream (M4). Declared, never probed.
    responses       text NOT NULL DEFAULT 'translate' CHECK (responses IN ('native', 'translate')),
    first_byte_secs integer NOT NULL DEFAULT 600 CHECK (first_byte_secs BETWEEN 1 AND 3600),
    chunk_secs      integer NOT NULL DEFAULT 120 CHECK (chunk_secs BETWEEN 1 AND 3600),
    enabled         boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- Public model ids share one namespace with personas (M3): the server
-- checks both tables before saving either.
CREATE TABLE models (
    id            uuid PRIMARY KEY,
    public_id     text NOT NULL UNIQUE,
    connection_id uuid NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    upstream_name text NOT NULL,
    enabled       boolean NOT NULL DEFAULT true,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (connection_id, upstream_name)
);

-- Bearer keys for /v1. Hash only; the display prefix is safe to show.
CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    principal_id uuid NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    name         text NOT NULL,
    key_sha256   bytea NOT NULL UNIQUE,
    prefix       text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz,
    revoked_at   timestamptz,
    last_used_at timestamptz
);
CREATE INDEX api_keys_principal ON api_keys (principal_id);

-- Who may use what. target_kind 'all' has no target; 'model' (and 'persona'
-- from M3) names one. No foreign key on target_id because it points at
-- different tables; deleting a model deletes its grants in the same
-- transaction.
CREATE TABLE grants (
    principal_id uuid NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    target_kind  text NOT NULL CHECK (target_kind IN ('all', 'model', 'persona')),
    target_id    uuid,
    created_at   timestamptz NOT NULL DEFAULT now(),
    CHECK ((target_kind = 'all') = (target_id IS NULL))
);
CREATE UNIQUE INDEX grants_all ON grants (principal_id) WHERE target_kind = 'all';
CREATE UNIQUE INDEX grants_target ON grants (principal_id, target_kind, target_id) WHERE target_kind <> 'all';

-- One row per proxied request. Counts and timings only: never prompts or
-- replies (AGENTS.md decision 6). Names are copied so history survives
-- renames and deletes.
CREATE TABLE usage (
    id                bigserial PRIMARY KEY,
    at                timestamptz NOT NULL,
    principal_id      uuid REFERENCES principals (id) ON DELETE SET NULL,
    key_id            uuid REFERENCES api_keys (id) ON DELETE SET NULL,
    model_id          uuid REFERENCES models (id) ON DELETE SET NULL,
    model_name        text NOT NULL,
    connection_id     uuid REFERENCES connections (id) ON DELETE SET NULL,
    endpoint          text NOT NULL,
    status            integer NOT NULL,
    prompt_tokens     bigint NOT NULL DEFAULT 0,
    completion_tokens bigint NOT NULL DEFAULT 0,
    cached_tokens     bigint NOT NULL DEFAULT 0,
    ttfb_ms           integer,
    total_ms          integer NOT NULL,
    usage_reported    boolean NOT NULL,
    client_closed     boolean NOT NULL DEFAULT false
);
CREATE INDEX usage_principal_at ON usage (principal_id, at);
CREATE INDEX usage_at ON usage (at);
