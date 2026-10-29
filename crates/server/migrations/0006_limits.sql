-- Limits per person, per-key scopes, and limit refusals in usage (M6).

-- NULL: no limit. They sit on the user row now and move to principals with
-- teams.
ALTER TABLE users ADD COLUMN rpm integer CHECK (rpm > 0);
ALTER TABLE users ADD COLUMN tokens_per_day bigint CHECK (tokens_per_day > 0);
ALTER TABLE users ADD COLUMN parallel_per_key integer DEFAULT 16 CHECK (parallel_per_key > 0);

-- A scoped key may use only its key_scopes rows, and only while its owner
-- still has the grant. Declared by the flag, not by the rows being there:
-- deleting the last model in a scope must leave a key that reaches nothing,
-- never one that reaches everything.
ALTER TABLE api_keys ADD COLUMN scoped boolean NOT NULL DEFAULT false;
CREATE TABLE key_scopes (
    key_id      uuid NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE,
    target_kind text NOT NULL CHECK (target_kind IN ('model', 'persona')),
    target_id   uuid NOT NULL,
    PRIMARY KEY (key_id, target_kind, target_id)
);

-- Which limit turned the request away (`rpm`, `tokens`, `parallel`).
ALTER TABLE usage ADD COLUMN limited text;
