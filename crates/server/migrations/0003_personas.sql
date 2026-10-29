-- Personas (layer 2): a system prompt and settings on a model, listed in
-- /v1/models next to the models. One public id namespace with `models`;
-- the server takes an advisory lock and checks both tables before saving.

-- Model-level defaults, same shape as persona params (core::persona).
ALTER TABLE models ADD COLUMN params jsonb NOT NULL DEFAULT '{}';

-- Which field a token limit default or lock goes out as, when the client
-- sent neither. Declared per connection: newer OpenAI models refuse
-- max_tokens, llama.cpp and others only know it.
ALTER TABLE connections ADD COLUMN token_field text NOT NULL DEFAULT 'max_tokens'
    CHECK (token_field IN ('max_tokens', 'max_completion_tokens'));

CREATE TABLE personas (
    id             uuid PRIMARY KEY,
    public_id      text NOT NULL UNIQUE,
    title          text NOT NULL,
    description    text NOT NULL DEFAULT '',
    -- RESTRICT: the database itself refuses removing a model in use.
    model_id       uuid NOT NULL REFERENCES models (id) ON DELETE RESTRICT,
    system_prompt  text NOT NULL DEFAULT '',
    prompt_mode    text NOT NULL DEFAULT 'prepend' CHECK (prompt_mode IN ('prepend', 'replace')),
    prompt_visible boolean NOT NULL DEFAULT true,
    params         jsonb NOT NULL DEFAULT '{}',
    enabled        boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX personas_model ON personas (model_id);

-- Every change worth undoing: model swaps, prompt edits, settings edits.
-- Admin-written prompts, never user conversations (decision 6). Names are
-- copied so history reads the same after renames and deletes.
CREATE TABLE persona_history (
    id              bigserial PRIMARY KEY,
    persona_id      uuid NOT NULL REFERENCES personas (id) ON DELETE CASCADE,
    at              timestamptz NOT NULL DEFAULT now(),
    by_user         uuid REFERENCES users (id) ON DELETE SET NULL,
    by_name         text NOT NULL,
    what            text NOT NULL CHECK (what IN ('created', 'model', 'prompt', 'settings')),
    from_model      uuid REFERENCES models (id) ON DELETE SET NULL,
    from_model_name text,
    to_model_name   text,
    prev_prompt     text,
    prev_mode       text,
    prev_params     jsonb
);
CREATE INDEX persona_history_persona ON persona_history (persona_id, at DESC);

-- model_name stays the model that answered; the persona is named beside it.
ALTER TABLE usage ADD COLUMN persona_id uuid REFERENCES personas (id) ON DELETE SET NULL;
ALTER TABLE usage ADD COLUMN persona_name text;
