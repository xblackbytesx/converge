-- Anthropic connections (M5). Per model: mark requests for Anthropic's
-- prompt caching (top-level `cache_control`). On by default; ignored on
-- `openai` connections.
ALTER TABLE models ADD COLUMN prompt_cache boolean NOT NULL DEFAULT true;
