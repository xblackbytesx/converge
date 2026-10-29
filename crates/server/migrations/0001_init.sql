-- Principals, people, web logins and invites.

-- Whoever owns keys and receives grants: a user now, a team later. Keys,
-- grants and limits always point here, never at users directly, so adding
-- teams needs new tables but changes none of these (AGENTS.md, Teams).
CREATE TABLE principals (
    id         uuid PRIMARY KEY,
    kind       text NOT NULL CHECK (kind IN ('user', 'team')),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A user's id is its principal's id.
CREATE TABLE users (
    id            uuid PRIMARY KEY REFERENCES principals (id) ON DELETE CASCADE,
    email         text NOT NULL UNIQUE,
    display_name  text NOT NULL DEFAULT '',
    role          text NOT NULL DEFAULT 'user' CHECK (role IN ('admin', 'user')),
    -- NULL: web login disabled until a password is set.
    password_hash text,
    tz            text NOT NULL DEFAULT 'UTC',
    created_at    timestamptz NOT NULL DEFAULT now(),
    disabled_at   timestamptz
);

-- Browser sessions: only the SHA-256 of the cookie value is stored.
CREATE TABLE web_sessions (
    token_sha256 bytea PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);
CREATE INDEX web_sessions_user ON web_sessions (user_id);

-- One-time invite links made by an admin. Hash only; single use, expiring.
CREATE TABLE invites (
    id           uuid PRIMARY KEY,
    token_sha256 bytea NOT NULL UNIQUE,
    role         text NOT NULL DEFAULT 'user' CHECK (role IN ('admin', 'user')),
    note         text NOT NULL DEFAULT '',
    created_by   uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz,
    used_by      uuid REFERENCES users (id) ON DELETE SET NULL
);
